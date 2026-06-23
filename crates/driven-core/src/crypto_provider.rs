//! Per-source crypto resolution (GA-blocking M5 surface).
//!
//! `encryption_enabled` is a PER-SOURCE setting (SPEC s2 `backup_sources`),
//! but the executor historically modelled crypto as ONE executor-wide
//! `Option<Arc<dyn SourceCryptoSuite>>` and branched on
//! `self.crypto.is_some()` (CODEX_NOTES "Per-source crypto resolution",
//! folded into the GA-blocking "CRYPTO SUITE PRODUCTION WIRING" gap). In a
//! mixed account that is wrong both ways: an encrypted source must NEVER
//! upload plaintext, and an unencrypted source must NEVER upload ciphertext.
//!
//! M5 replaces the single suite with a [`CryptoProvider`]: a seam the
//! executor consults PER SOURCE to resolve the suite to use. The production
//! implementation (`KeystoreCryptoProvider`, in the app shell) opens the
//! per-account [`Keystore`](driven_crypto::Keystore), loads the
//! [`MasterKey`](driven_crypto::MasterKey), and unwraps the per-source
//! [`SourceKey`](driven_crypto::SourceKey) into a
//! [`DrivenCryptoSuite`](driven_crypto::DrivenCryptoSuite).
//!
//! ## Fail-closed contract (GA-critical)
//!
//! A source whose `encryption_enabled` is `true` but whose key is
//! UNAVAILABLE (keychain locked, master key missing, unwrap failed) MUST
//! NOT fall back to plaintext. The provider distinguishes the two
//! "no suite" cases via [`CryptoResolution`]:
//! - [`CryptoResolution::Plaintext`] - the source is genuinely unencrypted;
//!   upload plaintext.
//! - [`CryptoResolution::Suite`] - an encrypted source with a resolved
//!   suite; upload ciphertext.
//! - [`CryptoResolution::Unavailable`] - an encrypted source whose key
//!   could not be resolved; the executor MUST fail the op closed (surface a
//!   `crypto.key_missing` error and skip the upload), never upload plaintext.

use std::sync::Arc;

use driven_crypto::SourceCryptoSuite;

use crate::types::SourceId;

/// The crypto decision for one source (M5 fail-closed contract).
///
/// Returned by [`CryptoProvider::resolve`]. The executor MUST treat the
/// three variants distinctly: only [`CryptoResolution::Plaintext`] permits a
/// plaintext upload; [`CryptoResolution::Unavailable`] is the FAIL-CLOSED
/// signal for an encryption-enabled source with no key.
pub enum CryptoResolution {
    /// The source is unencrypted (`encryption_enabled = false`): upload the
    /// plaintext bytes. This is the only variant that allows a plaintext
    /// upload.
    Plaintext,
    /// The source is encrypted and its per-source suite resolved: upload
    /// ciphertext through this suite.
    Suite(Arc<dyn SourceCryptoSuite>),
    /// The source is encrypted (`encryption_enabled = true`) but its key
    /// could not be resolved (keychain locked / master key missing / unwrap
    /// failed). The executor MUST FAIL CLOSED - error the op with
    /// `crypto.key_missing` and upload NOTHING. Never degrade to plaintext.
    Unavailable,
}

/// Resolves the encryption suite to use for a given source (M5 GA blocker).
///
/// Object-safe + `Send + Sync` so the executor holds an
/// `Option<Arc<dyn CryptoProvider>>` (`None` = no provider configured = every
/// source is plaintext, the test/unencrypted-only path). The production
/// `KeystoreCryptoProvider` (app shell) caches the resolved per-source suite
/// keyed by `source_id`.
pub trait CryptoProvider: Send + Sync {
    /// Resolve the crypto decision for `source_id` (see [`CryptoResolution`]).
    ///
    /// MUST be resolved PER source: a single account may mix encrypted and
    /// unencrypted sources. Implementations must return
    /// [`CryptoResolution::Unavailable`] (NOT [`CryptoResolution::Plaintext`])
    /// when the source is encryption-enabled but its key is unavailable, so
    /// the executor can fail closed (GA-critical).
    fn resolve(&self, source_id: &SourceId) -> CryptoResolution;

    /// Convenience: the suite for `source_id`, or `None` for a plaintext
    /// source. Provided so call sites that only need the `Suite`/`Plaintext`
    /// distinction stay terse; the FAIL-CLOSED path MUST use [`Self::resolve`]
    /// and branch on [`CryptoResolution::Unavailable`] explicitly. The
    /// default maps `Suite -> Some`, `Plaintext`/`Unavailable -> None` (so a
    /// caller using only this method treats an unavailable key as plaintext -
    /// which is exactly why the executor's upload path must NOT rely on it).
    fn suite_for(&self, source_id: &SourceId) -> Option<Arc<dyn SourceCryptoSuite>> {
        match self.resolve(source_id) {
            CryptoResolution::Suite(s) => Some(s),
            CryptoResolution::Plaintext | CryptoResolution::Unavailable => None,
        }
    }
}

/// A degenerate [`CryptoProvider`] that hands the SAME suite to every source.
///
/// This is the test / single-source adapter: it lets a test (or any caller
/// that already has one [`SourceCryptoSuite`]) plug into the per-source
/// `CryptoProvider` seam without standing up a keystore. It is NOT the
/// production resolver - the production `KeystoreCryptoProvider` (app shell)
/// resolves a DISTINCT suite per source and fails closed on a missing key.
///
/// Because it returns one suite for all source ids, it reproduces exactly the
/// pre-M5 "executor-wide suite" behaviour the existing crypto round-trip tests
/// assert, so those tests stay green through the M5 type refactor.
pub struct SingleSuiteProvider {
    suite: Arc<dyn SourceCryptoSuite>,
}

impl SingleSuiteProvider {
    /// Wrap one suite as a provider that returns it for every source.
    #[must_use]
    pub fn new(suite: Arc<dyn SourceCryptoSuite>) -> Self {
        Self { suite }
    }
}

impl CryptoProvider for SingleSuiteProvider {
    fn resolve(&self, _source_id: &SourceId) -> CryptoResolution {
        CryptoResolution::Suite(self.suite.clone())
    }
}
