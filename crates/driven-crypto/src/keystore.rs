//! OS-keychain persistence for the account [`MasterKey`] (DESIGN s7.1).
//!
//! The master key is the only secret stored in the OS keychain; per-source
//! keys live wrapped under it in the local SQLite state (see [`crate::key`]).
//! keyring 4.x's `v1` feature selects the platform-native store (macOS
//! Keychain, Windows Credential Manager, Linux Secret Service) automatically.
//! The 32 raw key bytes are stored via `set_secret`/`get_secret` (binary),
//! not `set_password` - key bytes are not valid UTF-8.

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
        self.entry.set_secret(key.as_bytes())?;
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
        let secret = match self.entry.get_secret() {
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

    /// Deletes the master key entry (account removal / encryption opt-out).
    /// Idempotent: a missing entry is not an error.
    ///
    /// # Errors
    /// Returns [`KeystoreError::Backend`] on a backend failure other than a
    /// missing entry.
    pub fn delete_master_key(&self) -> Result<(), KeystoreError> {
        match self.entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(KeystoreError::Backend(e)),
        }
    }
}
