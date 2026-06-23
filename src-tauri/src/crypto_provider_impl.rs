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

use driven_core::crypto_provider::{CryptoProvider, CryptoResolution};
use driven_core::state::SourceRow;
use driven_core::types::{AccountId, SourceId};
use driven_crypto::SourceCryptoSuite;
use std::sync::Mutex;

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
    /// TODO(M5 CRYPTO): implement per CODEX_NOTES "Per-source crypto
    /// resolution":
    /// - look up the [`SourceRow`] in `self.sources`; unknown id -> Plaintext
    ///   (no such encrypted source);
    /// - `encryption_enabled = false` -> cache + return Plaintext;
    /// - else open `Keystore::open(account_id)` + `load_master_key()`, take the
    ///   row's `wrapped_source_key`, `WrappedSourceKey::from_bytes` +
    ///   `MasterKey::unwrap_source_key`, build `DrivenCryptoSuite::new` -> cache
    ///   + return Suite;
    /// - ANY failure on the encrypted path (no master key, absent/short wrapped
    ///   key, unwrap/AEAD failure) -> cache + return Unavailable (FAIL CLOSED;
    ///   never Plaintext). Cache the verdict so the keystore is touched at most
    ///   once per source.
    fn resolve_cached(&self, _source_id: SourceId) -> Arc<CachedResolution> {
        let _ = (&self.account_id, &self.sources, &self.cache);
        todo!("M5 CRYPTO: open keystore -> load master key -> unwrap per-source key -> DrivenCryptoSuite; FAIL CLOSED on missing key; cache by source_id")
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
