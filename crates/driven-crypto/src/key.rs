//! Key material: the account [`MasterKey`], the per-source [`SourceKey`],
//! and the master-key wrapping of a source key (DESIGN s7.1).
//!
//! All key types `zeroize` their 32 bytes on drop so secrets do not linger
//! in freed heap/stack memory. Wrapping uses XChaCha20-Poly1305 with a
//! random 24-byte nonce stored alongside the ciphertext, matching the
//! `backup_sources.wrapped_source_key` column layout (DESIGN s7.1).

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use rand::TryRng;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::CryptoError;

/// Length in bytes of every Driven symmetric key (256-bit).
pub const KEY_LEN: usize = 32;

/// Length in bytes of an XChaCha20-Poly1305 nonce (192-bit).
pub const XNONCE_LEN: usize = 24;

/// The account-wide master key: 32 random bytes held in the OS keychain
/// (DESIGN s7.1). It never encrypts file content directly - it only wraps
/// per-source keys and seeds the BIP39 recovery phrase.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct MasterKey([u8; KEY_LEN]);

/// A per-source key: 32 random bytes, one per backup source, so ciphertext
/// is uncorrelatable across sources (DESIGN s7.1). Stored wrapped under the
/// [`MasterKey`]; unwrapped only in memory while a source syncs.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SourceKey([u8; KEY_LEN]);

impl MasterKey {
    /// Generates a fresh master key from the OS CSPRNG.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; KEY_LEN];
        // See `content.rs::ContentEncryptorImpl::new` for why `SysRng` needs
        // `try_fill_bytes` + `.expect()`: it's rand 0.10's fallible OS RNG,
        // and `.expect()` preserves rand 0.8 `OsRng::fill_bytes`'s panic on
        // the same underlying failure.
        rand::rngs::SysRng
            .try_fill_bytes(&mut bytes)
            .expect("OS RNG (getrandom) failed to fill master key");
        Self(bytes)
    }

    /// Constructs a master key from raw bytes (e.g. read back from the
    /// keychain or recovered from a BIP39 phrase). Takes ownership of the
    /// array so the caller can `zeroize` its own copy.
    #[must_use]
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// Borrows the raw key bytes (for keychain persistence / recovery-phrase
    /// encoding). Callers must not retain copies beyond their immediate use.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// Generates a new [`SourceKey`] and returns it wrapped under this master
    /// key (DESIGN s7.1). The returned [`WrappedSourceKey`] is what gets
    /// persisted in `backup_sources.wrapped_source_key`.
    ///
    /// # Errors
    /// Returns [`CryptoError::Protocol`] if the AEAD wrap fails (only on an
    /// allocation/implementation fault - inputs are always well-formed).
    pub fn wrap_new_source_key(&self) -> Result<(SourceKey, WrappedSourceKey), CryptoError> {
        let source_key = SourceKey::generate();
        let wrapped = self.wrap_source_key(&source_key)?;
        Ok((source_key, wrapped))
    }

    /// Wraps (encrypts) an existing source key under the master key with a
    /// fresh random nonce (DESIGN s7.1).
    ///
    /// # Errors
    /// Returns [`CryptoError::Protocol`] if the AEAD seal fails.
    pub fn wrap_source_key(&self, source_key: &SourceKey) -> Result<WrappedSourceKey, CryptoError> {
        let cipher = XChaCha20Poly1305::new(self.0.as_ref().into());
        let mut nonce_bytes = [0u8; XNONCE_LEN];
        rand::rngs::SysRng
            .try_fill_bytes(&mut nonce_bytes)
            .expect("OS RNG (getrandom) failed to fill wrap nonce");
        let nonce = XNonce::from_slice(&nonce_bytes);
        let ciphertext = cipher
            .encrypt(nonce, source_key.0.as_ref())
            .map_err(|_| CryptoError::Protocol("source key wrap failed".to_string()))?;
        Ok(WrappedSourceKey {
            nonce: nonce_bytes,
            ciphertext,
        })
    }

    /// Unwraps (decrypts) a source key previously wrapped under this master
    /// key (DESIGN s7.1).
    ///
    /// # Errors
    /// Returns [`CryptoError::DecryptFailed`] if the AEAD tag does not verify
    /// (wrong master key or corrupted wrapped blob), or
    /// [`CryptoError::Protocol`] if the plaintext is not exactly 32 bytes.
    pub fn unwrap_source_key(&self, wrapped: &WrappedSourceKey) -> Result<SourceKey, CryptoError> {
        let cipher = XChaCha20Poly1305::new(self.0.as_ref().into());
        let nonce = XNonce::from_slice(&wrapped.nonce);
        let mut plaintext = cipher
            .decrypt(nonce, wrapped.ciphertext.as_ref())
            .map_err(|_| CryptoError::DecryptFailed)?;
        if plaintext.len() != KEY_LEN {
            plaintext.zeroize();
            return Err(CryptoError::Protocol(
                "unwrapped source key has wrong length".to_string(),
            ));
        }
        let mut bytes = [0u8; KEY_LEN];
        bytes.copy_from_slice(&plaintext);
        plaintext.zeroize();
        Ok(SourceKey(bytes))
    }
}

impl SourceKey {
    /// Generates a fresh per-source key from the OS CSPRNG.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; KEY_LEN];
        rand::rngs::SysRng
            .try_fill_bytes(&mut bytes)
            .expect("OS RNG (getrandom) failed to fill source key");
        Self(bytes)
    }

    /// Constructs a source key from raw bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// Borrows the raw key bytes (for AEAD cipher construction).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

/// A source key encrypted under the account master key, plus the random
/// 24-byte nonce used (DESIGN s7.1). Serialised into
/// `backup_sources.wrapped_source_key` as `nonce || ciphertext`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrappedSourceKey {
    /// The random 24-byte XChaCha20-Poly1305 nonce used to wrap the key.
    pub nonce: [u8; XNONCE_LEN],
    /// The AEAD ciphertext (32-byte key + 16-byte tag = 48 bytes).
    pub ciphertext: Vec<u8>,
}

impl WrappedSourceKey {
    /// Serialises to the on-disk `nonce || ciphertext` byte layout.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(XNONCE_LEN + self.ciphertext.len());
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.ciphertext);
        out
    }

    /// Parses the `nonce || ciphertext` layout produced by [`Self::to_bytes`].
    ///
    /// # Errors
    /// Returns [`CryptoError::Protocol`] if the blob is shorter than a nonce
    /// plus a one-block AEAD ciphertext.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        // 32-byte key + 16-byte tag = 48 ciphertext bytes minimum.
        const MIN_CIPHERTEXT: usize = KEY_LEN + 16;
        if bytes.len() < XNONCE_LEN + MIN_CIPHERTEXT {
            return Err(CryptoError::Protocol(
                "wrapped source key blob too short".to_string(),
            ));
        }
        let mut nonce = [0u8; XNONCE_LEN];
        nonce.copy_from_slice(&bytes[..XNONCE_LEN]);
        Ok(Self {
            nonce,
            ciphertext: bytes[XNONCE_LEN..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_unwrap_round_trip() {
        let master = MasterKey::generate();
        let (source, wrapped) = master.wrap_new_source_key().unwrap();
        let unwrapped = master.unwrap_source_key(&wrapped).unwrap();
        assert_eq!(source.as_bytes(), unwrapped.as_bytes());
    }

    #[test]
    fn wrap_blob_serialisation_round_trip() {
        let master = MasterKey::generate();
        let (_source, wrapped) = master.wrap_new_source_key().unwrap();
        let blob = wrapped.to_bytes();
        let parsed = WrappedSourceKey::from_bytes(&blob).unwrap();
        assert_eq!(wrapped, parsed);
    }

    #[test]
    fn unwrap_with_wrong_master_fails() {
        let master = MasterKey::generate();
        let other = MasterKey::generate();
        let (_source, wrapped) = master.wrap_new_source_key().unwrap();
        assert!(matches!(
            other.unwrap_source_key(&wrapped),
            Err(CryptoError::DecryptFailed)
        ));
    }

    #[test]
    fn tampered_wrapped_key_fails() {
        let master = MasterKey::generate();
        let (_source, mut wrapped) = master.wrap_new_source_key().unwrap();
        wrapped.ciphertext[0] ^= 0xff;
        assert!(matches!(
            master.unwrap_source_key(&wrapped),
            Err(CryptoError::DecryptFailed)
        ));
    }

    #[test]
    fn short_blob_rejected() {
        assert!(matches!(
            WrappedSourceKey::from_bytes(&[0u8; 10]),
            Err(CryptoError::Protocol(_))
        ));
    }
}
