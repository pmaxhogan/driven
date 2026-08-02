//! OS-keychain persistence for the account [`MasterKey`] (DESIGN s7.1).
//!
//! The master key is the only secret stored in the OS keychain; per-source
//! keys live wrapped under it in the local SQLite state (see [`crate::key`]).
//! keyring 4.x's `v1` feature selects the platform-native store (macOS
//! Keychain, Windows Credential Manager, Linux Secret Service) automatically.
//! The 32 raw key bytes are stored via `set_secret`/`get_secret` (binary),
//! not `set_password` - key bytes are not valid UTF-8.
//!
//! ## Tests never touch a real keychain
//!
//! The round-trip tests below run against `keyring-core`'s in-memory mock,
//! installed by [`driven_test_fixtures::keychain::isolated`]. Never write a
//! test here that reaches the OS keychain: on macOS every `cargo test` rebuild
//! is a new binary identity, so such a test raises a modal permission prompt on
//! EVERY rebuild and blocks the run on it.

use keyring::Entry;
use zeroize::Zeroizing;

use crate::key::{MasterKey, KEY_LEN};

/// keyring "service" namespace for Driven master keys.
const KEYCHAIN_SERVICE: &str = "dev.maxhogan.driven";

/// Errors persisting / loading the master key from the OS keychain. Held
/// separate from [`CryptoError`] because keystore I/O is a setup-path
/// concern (the executor seam never touches the keychain - it gets an
/// already-unwrapped [`crate::key::SourceKey`]).
#[derive(Debug, thiserror::Error)]
pub enum KeystoreError {
    /// No master key is stored for the given account (first run, or the
    /// keychain was wiped - the recovery-phrase path applies).
    #[error("no master key in keychain for account")]
    NotFound,
    /// The stored secret was not exactly 32 bytes (corruption / foreign
    /// write).
    #[error("stored master key has wrong length: {0} bytes")]
    MalformedKey(usize),
    /// The underlying OS keychain backend failed.
    #[error("keychain backend error: {0}")]
    Backend(#[from] keyring::Error),
}

/// Stores, loads, and deletes the account master key in the OS keychain.
///
/// One [`Keystore`] corresponds to one Google account, keyed by
/// `account_id` (the keychain "username" within Driven's service
/// namespace), so multiple connected accounts each keep their own master
/// key (DESIGN s7.1).
pub struct Keystore {
    entry: Entry,
}

impl Keystore {
    /// Opens the keychain entry for one account. Does not yet touch the
    /// store beyond installing the platform default.
    ///
    /// # Errors
    /// Returns [`KeystoreError::Backend`] if the platform store cannot be
    /// initialised (e.g. headless Linux with no Secret Service).
    pub fn open(account_id: &str) -> Result<Self, KeystoreError> {
        let entry = Entry::new(KEYCHAIN_SERVICE, account_id)?;
        Ok(Self { entry })
    }

    /// Persists the master key to the OS keychain (overwrites any existing
    /// entry for this account).
    ///
    /// # Errors
    /// Returns [`KeystoreError::Backend`] on a keychain write failure.
    pub fn store_master_key(&self, key: &MasterKey) -> Result<(), KeystoreError> {
        keyring_off_runtime(|| self.entry.set_secret(key.as_bytes()))?;
        Ok(())
    }

    /// Loads the master key from the OS keychain.
    ///
    /// # Errors
    /// Returns [`KeystoreError::NotFound`] if no entry exists (map to the
    /// recovery-phrase flow), [`KeystoreError::MalformedKey`] if the stored
    /// blob is the wrong length, or [`KeystoreError::Backend`] on a backend
    /// failure.
    pub fn load_master_key(&self) -> Result<MasterKey, KeystoreError> {
        map_load_secret(keyring_off_runtime(|| self.entry.get_secret()))
    }

    /// Deletes the master key entry (account removal / encryption opt-out).
    /// Idempotent: a missing entry is not an error.
    ///
    /// # Errors
    /// Returns [`KeystoreError::Backend`] on a backend failure other than a
    /// missing entry.
    pub fn delete_master_key(&self) -> Result<(), KeystoreError> {
        map_delete_result(keyring_off_runtime(|| self.entry.delete_credential()))
    }
}

/// Run a keyring operation on a scratch thread on Linux.
///
/// The Linux `keyring` backend (Secret Service over DBus) internally
/// `block_on`s its own async runtime. Called from a tokio worker thread -
/// which is exactly where the app's IPC commands and assembly run - that
/// panics with "Cannot start a runtime from within a runtime", killing the
/// command mid-flight. The agent-QA e2e harness surfaced this as
/// `create_s3_account` hanging forever inside the Linux container (the panic
/// crash-dumped and the invoke promise never settled), and `driven-cli`
/// reproduced it directly. Keychain ops are rare and short-lived, so hopping
/// to a scoped scratch thread - which has no ambient runtime - is cheap,
/// dependency-free, and keeps every public signature sync. macOS / Windows
/// backends are plain native calls and stay on the calling thread.
fn keyring_off_runtime<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    #[cfg(target_os = "linux")]
    {
        std::thread::scope(|s| {
            s.spawn(f)
                .join()
                .expect("keyring operation thread panicked")
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        f()
    }
}

/// Maps a `keyring` `get_secret` result onto a loaded [`MasterKey`].
///
/// This is the keyring-result -> domain mapping that is Driven's own
/// responsibility (length validation, `NoEntry` -> recovery-phrase signal),
/// split out as a PURE free fn so the error mapping is unit-tested exhaustively
/// (including backend errors an in-memory store never produces) - the same
/// testability pattern as `driven-drive`'s `token_store::map_load_result`. The
/// happy path is additionally covered end-to-end by the round-trip tests
/// against the in-memory store. A missing
/// entry maps to [`KeystoreError::NotFound`]; a secret that is not exactly
/// [`KEY_LEN`] bytes maps to [`KeystoreError::MalformedKey`]; any other
/// backend error maps to [`KeystoreError::Backend`]. The retrieved bytes are
/// held in a [`Zeroizing`] buffer and scrubbed after the copy into the key.
fn map_load_secret(result: keyring::Result<Vec<u8>>) -> Result<MasterKey, KeystoreError> {
    let secret = match result {
        Ok(s) => Zeroizing::new(s),
        Err(keyring::Error::NoEntry) => return Err(KeystoreError::NotFound),
        Err(e) => return Err(KeystoreError::Backend(e)),
    };
    if secret.len() != KEY_LEN {
        return Err(KeystoreError::MalformedKey(secret.len()));
    }
    let mut bytes = [0u8; KEY_LEN];
    bytes.copy_from_slice(&secret);
    Ok(MasterKey::from_bytes(bytes))
}

/// Maps a `keyring` `delete_credential` result onto the idempotent-delete
/// domain result (pure, OS-keychain-free; mirrors `driven-drive`'s
/// `token_store::map_delete_result`). A missing entry is NOT an error
/// (delete is idempotent); any other backend failure maps to
/// [`KeystoreError::Backend`].
fn map_delete_result(result: keyring::Result<()>) -> Result<(), KeystoreError> {
    match result {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(KeystoreError::Backend(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_maps_no_entry_to_not_found() {
        // A missing keychain entry is the first-run / wiped-keychain signal the
        // recovery-phrase flow keys off - it must NOT be a generic backend error.
        assert!(matches!(
            map_load_secret(Err(keyring::Error::NoEntry)),
            Err(KeystoreError::NotFound)
        ));
    }

    #[test]
    fn load_maps_correct_length_secret_to_master_key() {
        // A well-formed 32-byte secret reconstructs the master key byte-for-byte.
        let raw = [7u8; KEY_LEN];
        let key = map_load_secret(Ok(raw.to_vec())).unwrap();
        assert_eq!(key.as_bytes(), &raw);
    }

    #[test]
    fn load_rejects_wrong_length_secret_as_malformed() {
        // A foreign / corrupt write of the wrong length must surface MalformedKey
        // carrying the observed length - never be silently truncated or panic.
        assert!(matches!(
            map_load_secret(Ok(vec![0u8; 16])),
            Err(KeystoreError::MalformedKey(16))
        ));
        assert!(matches!(
            map_load_secret(Ok(Vec::new())),
            Err(KeystoreError::MalformedKey(0))
        ));
        assert!(matches!(
            map_load_secret(Ok(vec![0u8; KEY_LEN + 1])),
            Err(KeystoreError::MalformedKey(n)) if n == KEY_LEN + 1
        ));
    }

    #[test]
    fn load_maps_other_backend_error() {
        // A real backend failure (anything but NoEntry) is preserved as Backend.
        let r = map_load_secret(Err(keyring::Error::Invalid(
            "service".to_string(),
            "boom".to_string(),
        )));
        assert!(matches!(r, Err(KeystoreError::Backend(_))));
    }

    #[test]
    fn delete_is_idempotent_for_missing_entry() {
        // Both a successful delete and a no-such-entry delete are Ok (idempotent).
        assert!(map_delete_result(Ok(())).is_ok());
        assert!(map_delete_result(Err(keyring::Error::NoEntry)).is_ok());
    }

    #[test]
    fn delete_surfaces_other_backend_error() {
        let r = map_delete_result(Err(keyring::Error::Invalid(
            "service".to_string(),
            "boom".to_string(),
        )));
        assert!(matches!(r, Err(KeystoreError::Backend(_))));
    }

    #[test]
    fn the_test_suite_is_isolated_from_the_os_keychain() {
        // The guard for this whole crate: if the isolation mechanism ever stops
        // working (a `keyring` upgrade changing how the default store is
        // installed, say), fail HERE and loudly rather than letting the tests
        // below quietly start writing into a real login keychain.
        assert!(
            driven_test_fixtures::keychain::is_isolated(),
            "the in-memory keyring store must be the effective default store"
        );
    }

    #[test]
    fn master_key_round_trips_through_the_keystore() {
        // The happy path of the real API (open -> store -> load), which the pure
        // mapping fns above cannot cover. Runs against the in-memory store.
        let Some(_g) = driven_test_fixtures::keychain::isolated() else {
            return;
        };
        let ks = Keystore::open("acct-crypto-round-trip").expect("open");
        let key = MasterKey::from_bytes([9u8; KEY_LEN]);
        ks.store_master_key(&key).expect("store");
        assert_eq!(
            ks.load_master_key().expect("load").as_bytes(),
            key.as_bytes()
        );
    }

    #[test]
    fn a_missing_master_key_is_not_found_and_delete_is_idempotent() {
        // First run / wiped keychain: NotFound is the recovery-phrase signal,
        // and account removal deletes unconditionally so a missing entry must
        // not error. Both over the real API rather than a constructed result.
        let Some(_g) = driven_test_fixtures::keychain::isolated() else {
            return;
        };
        let ks = Keystore::open("acct-crypto-absent").expect("open");
        assert!(matches!(ks.load_master_key(), Err(KeystoreError::NotFound)));
        ks.delete_master_key()
            .expect("deleting a missing key is a no-op");

        ks.store_master_key(&MasterKey::from_bytes([1u8; KEY_LEN]))
            .expect("store");
        ks.delete_master_key().expect("delete");
        assert!(
            matches!(ks.load_master_key(), Err(KeystoreError::NotFound)),
            "a deleted master key must not read back"
        );
    }

    #[test]
    fn master_keys_are_scoped_per_account() {
        // One Keystore per account (DESIGN s7.1): two connected accounts must
        // not read each other's master key.
        let Some(_g) = driven_test_fixtures::keychain::isolated() else {
            return;
        };
        let a = Keystore::open("acct-crypto-a").expect("open a");
        let b = Keystore::open("acct-crypto-b").expect("open b");
        a.store_master_key(&MasterKey::from_bytes([0xAAu8; KEY_LEN]))
            .expect("store a");
        b.store_master_key(&MasterKey::from_bytes([0xBBu8; KEY_LEN]))
            .expect("store b");
        assert_eq!(
            a.load_master_key().expect("load a").as_bytes(),
            &[0xAAu8; KEY_LEN]
        );
        assert_eq!(
            b.load_master_key().expect("load b").as_bytes(),
            &[0xBBu8; KEY_LEN]
        );
    }
}
