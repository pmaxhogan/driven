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
    /// Per-source metadata (`encryption_enabled`, `wrapped_source_key`),
    /// captured at assembly time, keyed by source id.
    sources: HashMap<SourceId, SourceRow>,
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
            sources,
            cache: Mutex::new(HashMap::new()),
        }
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
        let Some(row) = self.sources.get(&source_id) else {
            // Driven does not know this source -> nothing encrypted to protect.
            return CachedResolution::Plaintext;
        };

        if !row.encryption_enabled {
            return CachedResolution::Plaintext;
        }

        // Encryption-enabled from here on: every failure FAILS CLOSED.
        let Some(wrapped_bytes) = row.wrapped_source_key.as_ref() else {
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

impl CryptoProvider for KeystoreCryptoProvider {
    fn resolve(&self, source_id: &SourceId) -> CryptoResolution {
        match &*self.resolve_cached(*source_id) {
            CachedResolution::Plaintext => CryptoResolution::Plaintext,
            CachedResolution::Suite(s) => CryptoResolution::Suite(s.clone()),
            CachedResolution::Unavailable => CryptoResolution::Unavailable,
        }
    }
}
