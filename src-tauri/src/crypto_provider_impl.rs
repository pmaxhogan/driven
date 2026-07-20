//! [`KeystoreCryptoProvider`]: the production per-source crypto resolver
//! (M5 GA blocker, CODEX_NOTES "CRYPTO SUITE PRODUCTION WIRING" +
//! "Per-source crypto resolution").
//!
//! This is the one place that turns Driven's at-rest key material into a live
//! [`SourceCryptoSuite`]. For one account it:
//! 1. opens the per-account [`Keystore`](driven_crypto::Keystore) and loads
//!    the account [`MasterKey`](driven_crypto::MasterKey) (DESIGN s7.1);
//! 2. for each source, reads its `wrapped_source_key` + `encryption_enabled`
//!    (SPEC s2 `backup_sources`), unwraps the per-source
//!    [`SourceKey`](driven_crypto::SourceKey) under the master key
//!    ([`MasterKey::unwrap_source_key`]), and builds a
//!    [`DrivenCryptoSuite`](driven_crypto::DrivenCryptoSuite);
//! 3. caches the resolved suite keyed by `source_id` (DESIGN s7.1: one suite
//!    per source key, shared across upload tasks).
//!
//! ## Fail-closed (GA-critical)
//!
//! For a source with `encryption_enabled = true` whose key cannot be resolved
//! (keychain locked, master key missing, `wrapped_source_key` absent, unwrap
//! failed), [`CryptoProvider::resolve`] returns
//! [`CryptoResolution::Unavailable`] - NOT [`CryptoResolution::Plaintext`]. The
//! executor then fails the op closed (`crypto.key_missing`) and uploads
//! nothing. An `encryption_enabled = false` source resolves to
//! [`CryptoResolution::Plaintext`]. An encryption-enabled source must NEVER
//! upload plaintext and an unencrypted source must NEVER upload ciphertext.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use driven_core::crypto_provider::{CryptoProvider, CryptoResolution};
use driven_core::state::SourceRow;
use driven_core::types::{AccountId, SourceId};
use driven_crypto::{DrivenCryptoSuite, Keystore, MasterKey, SourceCryptoSuite, WrappedSourceKey};
use tracing::warn;

/// Tracing target for the keystore crypto provider.
const TARGET: &str = "driven::app::crypto";

/// The resolved-suite cache value for one source.
///
/// Distinguishes a genuinely-plaintext source from an encrypted source whose
/// key resolved vs one whose key is unavailable, so the cache itself carries
/// the fail-closed verdict (no re-deriving the keystore per op).
enum CachedResolution {
    /// `encryption_enabled = false`: upload plaintext.
    Plaintext,
    /// Encrypted source with a resolved suite.
    Suite(Arc<dyn SourceCryptoSuite>),
    /// Encrypted source whose key is unavailable: FAIL CLOSED.
    Unavailable,
}

/// Production [`CryptoProvider`] over the per-account keystore (M5).
///
/// One instance per account. Holds the source metadata needed to resolve a
/// suite plus a `source_id`-keyed cache. `Send + Sync`: the executor holds it
/// behind `Arc<dyn CryptoProvider>` and resolves concurrently from upload
/// tasks (the cache is behind a recoverable [`Mutex`]).
pub struct KeystoreCryptoProvider {
    /// The account whose master key unwraps these sources' keys.
    account_id: AccountId,
    /// Per-source metadata (`encryption_enabled`, `wrapped_source_key`), keyed
    /// by source id.
    ///
    /// B2 (LIVE, not boot-snapshot): behind a recoverable [`Mutex`] so the
    /// source-command layer can REFRESH it ([`Self::refresh`]) when a source is
    /// added / toggled / removed while the app runs. Before this fix the rows
    /// were captured once at assembly, so an encrypted source added mid-session
    /// resolved `Unavailable` (no row -> fail closed) until restart. Now the
    /// reconfigure path refreshes this map so the new source's key resolves on
    /// the next tick (ROADMAP M6 acceptance). Fail-closed is preserved: a source
    /// not in the map is treated as Driven-doesn't-own-it -> Plaintext, and an
    /// encryption-enabled source whose key cannot be resolved is `Unavailable`.
    sources: Mutex<HashMap<SourceId, SourceRow>>,
    /// Resolved-suite cache keyed by source id (DESIGN s7.1).
    cache: Mutex<HashMap<SourceId, Arc<CachedResolution>>>,
}

impl KeystoreCryptoProvider {
    /// Build a provider for `account_id` over its `sources`.
    ///
    /// Does NOT touch the keychain yet - the master-key load + per-source
    /// unwrap happen lazily on first [`CryptoProvider::resolve`] for a source
    /// (and are cached). Capturing the rows up front avoids a `StateRepo`
    /// round-trip on the hot upload path.
    #[must_use]
    pub fn new(account_id: AccountId, sources: Vec<SourceRow>) -> Self {
        let sources = sources.into_iter().map(|s| (s.id, s)).collect();
        Self {
            account_id,
            sources: Mutex::new(sources),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// B2: REFRESH the per-source metadata to the current set (called by the
    /// source-command layer after a source add / update / remove so a change
    /// made WHILE the app runs is picked up on the next tick, without a restart).
    ///
    /// Replaces the live source map AND drops any cached resolution whose row
    /// changed or vanished, so:
    /// - a NEW encrypted source's key now resolves (was `Unavailable` -> no row);
    /// - a source whose `wrapped_source_key` / `encryption_enabled` changed
    ///   re-resolves against the new row;
    /// - a removed source's stale cache entry is dropped.
    ///
    /// Fail-closed is preserved throughout (a missing key still yields
    /// `Unavailable`, never plaintext).
    pub fn refresh(&self, sources: Vec<SourceRow>) {
        let new_map: HashMap<SourceId, SourceRow> =
            sources.into_iter().map(|s| (s.id, s)).collect();
        // Invalidate cache entries that no longer match the new rows so the next
        // resolve recomputes the verdict from current metadata.
        {
            let old = self.lock_sources();
            let mut cache = self.lock_cache();
            cache.retain(|id, _| matches!((old.get(id), new_map.get(id)), (Some(o), Some(n)) if rows_crypto_eq(o, n)));
        }
        *self.lock_sources() = new_map;
    }

    /// Lock the source map, recovering a poisoned lock.
    fn lock_sources(&self) -> std::sync::MutexGuard<'_, HashMap<SourceId, SourceRow>> {
        self.sources.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Resolve (and cache) one source's crypto decision.
    ///
    /// Implements the CODEX_NOTES "Per-source crypto resolution" contract:
    /// - look up the [`SourceRow`] in `self.sources`; an unknown id resolves to
    ///   `Plaintext` (Driven does not own / encrypt it, so there is nothing to
    ///   fail closed over);
    /// - `encryption_enabled = false` -> `Plaintext`;
    /// - else open [`Keystore::open`] + [`Keystore::load_master_key`], parse the
    ///   row's `wrapped_source_key` ([`WrappedSourceKey::from_bytes`]), unwrap
    ///   the per-source key ([`MasterKey::unwrap_source_key`], DESIGN s7.1), and
    ///   build a [`DrivenCryptoSuite`] -> `Suite`;
    /// - ANY failure on the encrypted path (keychain unavailable, no master key,
    ///   absent / malformed wrapped key, unwrap / AEAD failure) -> `Unavailable`
    ///   (FAIL CLOSED; NEVER `Plaintext`). The executor then errors the op
    ///   `crypto.key_missing` and uploads nothing.
    ///
    /// The resolved verdict is cached by `source_id` so the keychain is touched
    /// at most once per source for the process lifetime.
    fn resolve_cached(&self, source_id: SourceId) -> Arc<CachedResolution> {
        // Fast path: return the already-resolved verdict if present.
        {
            let cache = self.lock_cache();
            if let Some(hit) = cache.get(&source_id) {
                return hit.clone();
            }
        }

        let resolution = Arc::new(self.resolve_uncached(source_id));

        // V5-P2-1 / C5-P2-2: cache ONLY the STABLE verdicts (Plaintext, Suite).
        // Do NOT memoize `Unavailable`: it is a TRANSIENT condition (keychain /
        // Secret-Service locked at autostart, a temporarily missing key). Caching
        // it would strand an encrypted source as un-backupable until the app
        // restarts, even after the keychain unlocks. Returning it WITHOUT caching
        // makes the next op re-attempt the unwrap (fail-closed is preserved -
        // the op still errors `crypto.key_missing` until the key is available).
        if matches!(*resolution, CachedResolution::Unavailable) {
            return resolution;
        }

        // Store under the lock. A concurrent resolver for the same id may have
        // raced us; keep whichever landed first (both compute the same verdict
        // from the same immutable row + keystore, so either is correct).
        let mut cache = self.lock_cache();
        cache
            .entry(source_id)
            .or_insert_with(|| resolution.clone())
            .clone()
    }

    /// Lock the cache, recovering a poisoned lock instead of panicking (house
    /// rule: no `unwrap`/`expect`/`panic!` in non-test code).
    fn lock_cache(&self) -> std::sync::MutexGuard<'_, HashMap<SourceId, Arc<CachedResolution>>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Compute the crypto verdict for `source_id` WITHOUT consulting / writing
    /// the cache (the keystore-touching core of [`Self::resolve_cached`]).
    fn resolve_uncached(&self, source_id: SourceId) -> CachedResolution {
        // B2: read the row from the LIVE map (refreshed on source changes), and
        // clone out the two crypto-relevant fields so the lock is not held
        // across the keystore work below.
        let (encryption_enabled, wrapped_bytes) = {
            let sources = self.lock_sources();
            let Some(row) = sources.get(&source_id) else {
                // Driven does not know this source -> nothing encrypted to protect.
                return CachedResolution::Plaintext;
            };
            (row.encryption_enabled, row.wrapped_source_key.clone())
        };

        if !encryption_enabled {
            return CachedResolution::Plaintext;
        }

        // Encryption-enabled from here on: every failure FAILS CLOSED.
        let Some(wrapped_bytes) = wrapped_bytes.as_ref() else {
            warn!(
                target: TARGET,
                account_id = %self.account_id,
                %source_id,
                "encryption enabled but wrapped_source_key is absent; failing closed"
            );
            return CachedResolution::Unavailable;
        };

        let keystore = match Keystore::open(&self.account_id.to_string()) {
            Ok(k) => k,
            Err(e) => {
                warn!(
                    target: TARGET,
                    account_id = %self.account_id,
                    %source_id,
                    error = %e,
                    "keystore open failed; failing closed"
                );
                return CachedResolution::Unavailable;
            }
        };

        let master_key: MasterKey = match keystore.load_master_key() {
            Ok(k) => k,
            Err(e) => {
                warn!(
                    target: TARGET,
                    account_id = %self.account_id,
                    %source_id,
                    error = %e,
                    "master key unavailable; failing closed (no plaintext fallback)"
                );
                return CachedResolution::Unavailable;
            }
        };

        let wrapped = match WrappedSourceKey::from_bytes(wrapped_bytes) {
            Ok(w) => w,
            Err(e) => {
                warn!(
                    target: TARGET,
                    account_id = %self.account_id,
                    %source_id,
                    error = %e,
                    "wrapped source key malformed; failing closed"
                );
                return CachedResolution::Unavailable;
            }
        };

        let source_key = match master_key.unwrap_source_key(&wrapped) {
            Ok(k) => k,
            Err(e) => {
                warn!(
                    target: TARGET,
                    account_id = %self.account_id,
                    %source_id,
                    error = %e,
                    "source key unwrap failed; failing closed"
                );
                return CachedResolution::Unavailable;
            }
        };

        // The suite owns the source key + derived filename sub-keys and zeroizes
        // all key material on drop (DESIGN s7.1).
        CachedResolution::Suite(Arc::new(DrivenCryptoSuite::new(source_key)))
    }
}

/// B2: two source rows resolve to the SAME crypto verdict iff their
/// `encryption_enabled` flag and `wrapped_source_key` bytes match. Used by
/// [`KeystoreCryptoProvider::refresh`] to decide which cached resolutions
/// survive a metadata refresh (a row whose crypto fields are unchanged keeps its
/// cached suite; any change drops the entry so it re-resolves).
fn rows_crypto_eq(a: &SourceRow, b: &SourceRow) -> bool {
    a.encryption_enabled == b.encryption_enabled && a.wrapped_source_key == b.wrapped_source_key
}

impl CryptoProvider for KeystoreCryptoProvider {
    fn resolve(&self, source_id: &SourceId) -> CryptoResolution {
        match &*self.resolve_cached(*source_id) {
            CachedResolution::Plaintext => CryptoResolution::Plaintext,
            CachedResolution::Suite(s) => CryptoResolution::Suite(s.clone()),
            CachedResolution::Unavailable => CryptoResolution::Unavailable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use driven_core::state::SourceRow;
    use driven_core::types::{AccountId, SourceId};

    /// A source row with the given encryption settings (the only crypto-relevant
    /// fields; the rest are filler).
    fn source(
        id: SourceId,
        account: AccountId,
        encrypted: bool,
        wrapped: Option<Vec<u8>>,
    ) -> SourceRow {
        SourceRow {
            id,
            account_id: account,
            display_name: String::new(),
            enabled: true,
            local_path: "/tmp/x".to_string(),
            drive_folder_id: "root".to_string(),
            drive_id: None,
            drive_folder_path: String::new(),
            encryption_enabled: encrypted,
            wrapped_source_key: wrapped,
            respect_gitignore: true,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            placeholder_policy: Default::default(),
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: None,
            last_deep_verify_at: None,
            created_at: 0,
        }
    }

    #[test]
    fn unknown_source_resolves_plaintext() {
        let account = AccountId::new_v4();
        let provider = KeystoreCryptoProvider::new(account, Vec::new());
        // Driven does not own this source id -> nothing to fail closed over.
        assert!(matches!(
            provider.resolve(&SourceId::new_v4()),
            CryptoResolution::Plaintext
        ));
    }

    #[test]
    fn unencrypted_source_resolves_plaintext() {
        let account = AccountId::new_v4();
        let sid = SourceId::new_v4();
        let provider =
            KeystoreCryptoProvider::new(account, vec![source(sid, account, false, None)]);
        assert!(matches!(
            provider.resolve(&sid),
            CryptoResolution::Plaintext
        ));
    }

    #[test]
    fn encrypted_source_without_wrapped_key_fails_closed() {
        // GA-critical: encryption-enabled but no key -> Unavailable, NEVER
        // Plaintext (the executor then errors crypto.key_missing).
        let account = AccountId::new_v4();
        let sid = SourceId::new_v4();
        let provider = KeystoreCryptoProvider::new(account, vec![source(sid, account, true, None)]);
        assert!(matches!(
            provider.resolve(&sid),
            CryptoResolution::Unavailable
        ));
    }

    #[test]
    fn refresh_picks_up_a_newly_added_encrypted_source() {
        // B2: a source added WHILE the app runs must be SEEN by the live provider
        // after a refresh - it must NOT stay "unknown -> Plaintext" (which would
        // upload an encrypted source's bytes in the clear). Before refresh the id
        // is unknown (Plaintext); after refresh adding it as encrypted-without-key
        // it FAILS CLOSED (Unavailable), proving the new row is now live.
        let account = AccountId::new_v4();
        let sid = SourceId::new_v4();
        let provider = KeystoreCryptoProvider::new(account, Vec::new());
        assert!(
            matches!(provider.resolve(&sid), CryptoResolution::Plaintext),
            "unknown source before refresh"
        );

        provider.refresh(vec![source(sid, account, true, None)]);
        assert!(
            matches!(provider.resolve(&sid), CryptoResolution::Unavailable),
            "after refresh the new encrypted source is live and fails closed (no key)"
        );
    }

    #[test]
    fn refresh_invalidates_cache_when_a_source_toggles_encryption() {
        // A source that was unencrypted (cached Plaintext) and is later toggled to
        // encrypted must re-resolve (to Unavailable here, since no key) - the
        // stale Plaintext cache entry must be dropped on refresh.
        let account = AccountId::new_v4();
        let sid = SourceId::new_v4();
        let provider =
            KeystoreCryptoProvider::new(account, vec![source(sid, account, false, None)]);
        assert!(matches!(
            provider.resolve(&sid),
            CryptoResolution::Plaintext
        ));

        provider.refresh(vec![source(sid, account, true, None)]);
        assert!(
            matches!(provider.resolve(&sid), CryptoResolution::Unavailable),
            "toggling to encrypted must invalidate the cached Plaintext verdict"
        );
    }

    #[test]
    fn refresh_drops_a_removed_source_to_plaintext() {
        // A removed source's stale cache entry must be dropped; it then resolves as
        // unknown -> Plaintext (Driven no longer owns it).
        let account = AccountId::new_v4();
        let sid = SourceId::new_v4();
        let provider = KeystoreCryptoProvider::new(account, vec![source(sid, account, true, None)]);
        assert!(matches!(
            provider.resolve(&sid),
            CryptoResolution::Unavailable
        ));
        provider.refresh(Vec::new());
        assert!(matches!(
            provider.resolve(&sid),
            CryptoResolution::Plaintext
        ));
    }
}
