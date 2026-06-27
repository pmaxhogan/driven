//! `driven-crypto` - Driven's authenticated-encryption format.
//!
//! Owns master/per-source key types, OS-keychain key storage,
//! per-path filename encryption (XChaCha20-Poly1305 + base32hex), chunked
//! content encryption (XChaCha20-Poly1305 STREAM via
//! `chacha20poly1305::aead::stream::EncryptorBE32`), and BIP39
//! recovery-phrase encoding of the master key (DESIGN s7, CRYPTO_FORMAT).
//!
//! ## The executor seam
//!
//! The M3 executor (in `driven-core`) encrypts content on the read path
//! and encrypts filenames before issuing a Drive create. It codes against
//! the [`SourceCryptoSuite`] trait declared here, NOT against the concrete
//! cipher. To keep the crate dependency graph one-way (`driven-core`
//! depends on `driven-crypto`, never the reverse), **this trait must not
//! reference any `driven-core` type**: every signature is in terms of
//! `&str` / [`Bytes`] / `[u8; 16]` and the crate-local [`CryptoError`].
//! The executor maps a [`CryptoError`] onto the core `ErrorCode`
//! (`crypto.*`) at the call site.
//!
//! Content encryption is *stateful* (STREAM keeps a per-file 32-bit chunk
//! counter and a last-chunk flag; the ciphertext MD5 Drive verifies is
//! only known once the final chunk is sealed). So the seam yields a
//! per-file [`ContentEncryptor`] / [`ContentDecryptor`] object rather than
//! a one-shot `encrypt(stream) -> stream`, which could not return the MD5
//! cleanly. Filename encryption is per path-component, with the parent
//! component's ciphertext as AEAD AAD (DESIGN s7), so it takes the parent
//! AAD explicitly.

use bytes::Bytes;

pub mod content;
pub mod filename;
pub mod key;
pub mod keystore;
pub mod recovery;

pub use content::{ContentDecryptorImpl, ContentEncryptorImpl, CONTENT_MAGIC, HEADER_LEN};
pub use key::{MasterKey, SourceKey, WrappedSourceKey, KEY_LEN};
pub use keystore::{Keystore, KeystoreError};
pub use recovery::{master_key_to_phrase, phrase_to_master_key};

/// Errors the crypto seam surfaces to the executor (mapped to the core
/// `crypto.*` `ErrorCode` at the boundary - this enum deliberately holds
/// no `driven-core` type so the dependency graph stays one-way).
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// The per-source / master key could not be found in the OS keychain
    /// (maps to `crypto.key_missing`).
    #[error("encryption key not found in keychain")]
    KeyMissing,
    /// AEAD verification failed on decrypt - wrong key, corrupted
    /// ciphertext, or a reorder/truncate the STREAM construction caught
    /// (maps to `crypto.decrypt_failed`).
    #[error("AEAD verification failed")]
    DecryptFailed,
    /// A supplied BIP39 recovery phrase failed its checksum (maps to
    /// `crypto.recovery_phrase_invalid`).
    #[error("recovery phrase failed BIP39 checksum")]
    RecoveryPhraseInvalid,
    /// The encryptor was used out of contract (e.g. a chunk pushed after
    /// `finalize`, or a header read that ran short). Indicates a caller or
    /// format bug.
    #[error("crypto stream protocol violation: {0}")]
    Protocol(String),
}

/// A per-file content encryptor implementing the XChaCha20-Poly1305 STREAM
/// construction (DESIGN s7.1).
///
/// Lifecycle, driven by the executor's CPU-pipeline stage (DESIGN
/// s11.4.3):
/// 1. [`Self::header`] - emit the 40-byte `magic | nonce` file header that
///    must be the first bytes of the Drive object's body.
/// 2. [`Self::encrypt_chunk`] once per plaintext chunk (64 KiB plaintext
///    -> 64 KiB + 16-byte tag ciphertext), in order. The STREAM 32-bit
///    big-endian counter advances internally.
/// 3. [`Self::finalize_last`] for the final chunk (sets the STREAM
///    last-chunk flag) and consume the encryptor, returning the trailing
///    ciphertext and the MD5 over **all** ciphertext bytes emitted
///    (header + every chunk) - the value compared against Drive's
///    `md5Checksum` (DESIGN s7.1 "Drive's md5Checksum is the ciphertext
///    md5").
///
/// Implementations accumulate the ciphertext MD5 across `header`,
/// `encrypt_chunk`, and `finalize_last`, so the executor must feed exactly
/// the bytes it uploads, in order.
pub trait ContentEncryptor: Send {
    /// Returns the fixed-size file header (`magic` + per-file random
    /// nonce, 40 bytes per DESIGN s7.1) that prefixes the ciphertext body.
    /// Must be called once, before any [`Self::encrypt_chunk`].
    fn header(&mut self) -> Bytes;

    /// Encrypts one non-final plaintext chunk, returning its ciphertext
    /// (plaintext length + 16-byte tag). Advances the STREAM counter.
    fn encrypt_chunk(&mut self, plaintext: &[u8]) -> Result<Bytes, CryptoError>;

    /// Encrypts the final plaintext chunk with the STREAM last-chunk flag
    /// set, consuming the encryptor. Returns the final ciphertext and the
    /// MD5 over every ciphertext byte emitted (header + all chunks), for
    /// the Drive `md5Checksum` verification (DESIGN s7.1).
    fn finalize_last(self: Box<Self>, plaintext: &[u8]) -> Result<(Bytes, [u8; 16]), CryptoError>;
}

/// A per-file content decryptor: the inverse of [`ContentEncryptor`], used
/// by the Restore browser (DESIGN s7.3).
///
/// The executor / restore sink reads the 40-byte header first (handed to
/// the suite via [`SourceCryptoSuite::content_decryptor`]), then streams
/// ciphertext chunks through [`Self::decrypt_chunk`], marking the final
/// chunk with [`Self::decrypt_last`]. A failed AEAD tag (wrong key, a
/// reorder, a truncation) surfaces [`CryptoError::DecryptFailed`].
pub trait ContentDecryptor: Send {
    /// Decrypts one non-final ciphertext chunk back to plaintext.
    fn decrypt_chunk(&mut self, ciphertext: &[u8]) -> Result<Bytes, CryptoError>;

    /// Decrypts the final ciphertext chunk (STREAM last-chunk flag
    /// expected), consuming the decryptor.
    fn decrypt_last(self: Box<Self>, ciphertext: &[u8]) -> Result<Bytes, CryptoError>;
}

/// The encryption contract the M3 executor codes against (DESIGN s7).
///
/// One instance corresponds to one source's per-source key (so cross-source
/// ciphertext is uncorrelatable, DESIGN s7.1). When a source has
/// encryption disabled the executor holds `None` instead of a suite and
/// uploads plaintext.
///
/// Object-safe and `Send + Sync` so the orchestrator can hold an
/// `Option<std::sync::Arc<dyn SourceCryptoSuite>>` (the SPEC s5
/// `Option<SourceCryptoSuite>` field; SPEC s5 is explicitly illustrative
/// on the exact spelling - see the M3 phase-1 finding).
pub trait SourceCryptoSuite: Send + Sync {
    /// Opens a fresh per-file [`ContentEncryptor`] with a newly generated
    /// file nonce (DESIGN s7.1). Called once per uploaded file.
    fn content_encryptor(&self) -> Box<dyn ContentEncryptor>;

    /// Opens a [`ContentDecryptor`] for a file, given the 40-byte header
    /// (`magic | nonce`) read from the start of the Drive object
    /// (DESIGN s7.1, s7.3). Returns [`CryptoError::Protocol`] if the
    /// header is malformed.
    fn content_decryptor(&self, header: &[u8]) -> Result<Box<dyn ContentDecryptor>, CryptoError>;

    /// Encrypts one path component, returning the base32hex (RFC 4648 s7,
    /// lowercase, no padding) ciphertext usable as a Drive filename
    /// (DESIGN s7.1 filename encryption).
    ///
    /// `component` is the single NFC-canonical path segment (the executor
    /// canonicalises before calling). `parent_ciphertext_aad` is the
    /// parent folder's already-encrypted ciphertext name, bound in as the
    /// AEAD AAD so moving a folder re-derives its children's ciphertext
    /// (DESIGN s7.1). For a top-level component pass an empty slice.
    /// Deterministic: the same `(component, parent_aad)` always yields the
    /// same ciphertext (needed for `files.list` lookups).
    fn encrypt_filename(
        &self,
        component: &str,
        parent_ciphertext_aad: &[u8],
    ) -> Result<String, CryptoError>;

    /// Inverse of [`Self::encrypt_filename`]: decodes the base32hex
    /// ciphertext name and AEAD-decrypts it back to the plaintext path
    /// component (DESIGN s7.3 restore browser shows plaintext names). The
    /// same `parent_ciphertext_aad` used to encrypt must be supplied.
    fn decrypt_filename(
        &self,
        ciphertext_name: &str,
        parent_ciphertext_aad: &[u8],
    ) -> Result<String, CryptoError>;
}

/// The concrete [`SourceCryptoSuite`] the executor holds, bound to one
/// source's per-source key (DESIGN s7.1).
///
/// Construct it from an unwrapped [`SourceKey`] (the orchestrator unwraps
/// the source key off the [`MasterKey`] via
/// [`MasterKey::unwrap_source_key`] at source-start). It owns the source
/// key and the two BLAKE3-derived filename sub-keys; all key material is
/// zeroized on drop. Wrap it in an `Arc` to share across the executor's
/// upload tasks (the trait is `Send + Sync`).
pub struct DrivenCryptoSuite {
    source_key: key::SourceKey,
    filename_keys: filename::FilenameKeys,
}

impl DrivenCryptoSuite {
    /// Builds a suite for one source from its unwrapped per-source key. The
    /// filename sub-keys are derived once up front (DESIGN s7.1).
    #[must_use]
    pub fn new(source_key: key::SourceKey) -> Self {
        let filename_keys = filename::FilenameKeys::derive(&source_key);
        Self {
            source_key,
            filename_keys,
        }
    }
}

impl SourceCryptoSuite for DrivenCryptoSuite {
    fn content_encryptor(&self) -> Box<dyn ContentEncryptor> {
        Box::new(content::ContentEncryptorImpl::new(&self.source_key))
    }

    fn content_decryptor(&self, header: &[u8]) -> Result<Box<dyn ContentDecryptor>, CryptoError> {
        let dec = content::ContentDecryptorImpl::from_header(&self.source_key, header)?;
        Ok(Box::new(dec))
    }

    fn encrypt_filename(
        &self,
        component: &str,
        parent_ciphertext_aad: &[u8],
    ) -> Result<String, CryptoError> {
        self.filename_keys.encrypt(component, parent_ciphertext_aad)
    }

    fn decrypt_filename(
        &self,
        ciphertext_name: &str,
        parent_ciphertext_aad: &[u8],
    ) -> Result<String, CryptoError> {
        self.filename_keys
            .decrypt(ciphertext_name, parent_ciphertext_aad)
    }
}

#[cfg(test)]
mod suite_tests {
    use super::*;

    fn suite() -> DrivenCryptoSuite {
        DrivenCryptoSuite::new(key::SourceKey::generate())
    }

    #[test]
    fn content_round_trip_via_suite() {
        let s = suite();
        let mut enc = s.content_encryptor();
        let header = enc.header();
        let c0 = enc.encrypt_chunk(b"chunk-zero").unwrap();
        let (c1, _md5) = enc.finalize_last(b"chunk-last").unwrap();

        let mut dec = s.content_decryptor(&header).unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(&dec.decrypt_chunk(&c0).unwrap());
        out.extend_from_slice(&dec.decrypt_last(&c1).unwrap());
        assert_eq!(out, b"chunk-zerochunk-last");
    }

    #[test]
    fn filename_round_trip_via_suite() {
        let s = suite();
        let parent = s.encrypt_filename("dir", &[]).unwrap();
        let child = s.encrypt_filename("file.txt", parent.as_bytes()).unwrap();
        assert_eq!(
            s.decrypt_filename(&child, parent.as_bytes()).unwrap(),
            "file.txt"
        );
    }

    #[test]
    fn suite_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DrivenCryptoSuite>();
    }

    #[test]
    fn bad_header_via_suite_errors() {
        let s = suite();
        assert!(matches!(
            s.content_decryptor(&[0u8; 4]),
            Err(CryptoError::Protocol(_))
        ));
    }

    #[test]
    fn full_keychain_loss_recovery_decrypts_old_ciphertext() {
        // The end-to-end disaster-recovery promise (DESIGN s7.3): a user whose OS
        // keychain is wiped (machine reformat) can paste back their 24-word BIP39
        // phrase and STILL decrypt everything previously uploaded. This ties
        // together the four pieces each unit-tested in isolation - master-key
        // recovery phrase, master-wraps-source, content STREAM, and filename
        // encryption - in the exact order the recovery flow exercises them.
        //
        // 1. Original install: a master key wraps a fresh per-source key; the
        //    wrapped blob is what persists in SQLite (`wrapped_source_key`), the
        //    master key is what lived ONLY in the now-lost keychain.
        let master = MasterKey::generate();
        let (source_key, wrapped) = master.wrap_new_source_key().unwrap();
        let wrapped_blob = wrapped.to_bytes(); // the on-disk form
        let phrase = master_key_to_phrase(&master).unwrap(); // what the user wrote down

        // 2. Encrypt a file + its path under the original source key, capturing the
        //    header, ciphertext chunks, and the encrypted folder/leaf names.
        let suite = DrivenCryptoSuite::new(source_key);
        let dir_name = suite.encrypt_filename("Taxes", &[]).unwrap();
        let leaf_name = suite
            .encrypt_filename("2023-return.pdf", dir_name.as_bytes())
            .unwrap();
        let mut enc = suite.content_encryptor();
        let header = enc.header();
        let c0 = enc.encrypt_chunk(b"page one of the return").unwrap();
        let (c1, _md5) = enc.finalize_last(b"and the final page").unwrap();
        // Drop the original suite + source key, modelling the wiped keychain: from
        // here on ONLY the phrase and the on-disk wrapped blob exist.
        drop(suite);

        // 3. Recover: phrase -> master key -> unwrap the SAME source key from the
        //    persisted blob -> rebuild the suite.
        let recovered_master = phrase_to_master_key(&phrase).unwrap();
        let restored_wrapped = WrappedSourceKey::from_bytes(&wrapped_blob).unwrap();
        let recovered_source = recovered_master
            .unwrap_source_key(&restored_wrapped)
            .unwrap();
        let recovered_suite = DrivenCryptoSuite::new(recovered_source);

        // 4. The recovered suite decrypts both the plaintext path components and
        //    the file content that the lost-key suite produced.
        assert_eq!(
            recovered_suite.decrypt_filename(&dir_name, &[]).unwrap(),
            "Taxes"
        );
        assert_eq!(
            recovered_suite
                .decrypt_filename(&leaf_name, dir_name.as_bytes())
                .unwrap(),
            "2023-return.pdf"
        );
        let mut dec = recovered_suite.content_decryptor(&header).unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(&dec.decrypt_chunk(&c0).unwrap());
        out.extend_from_slice(&dec.decrypt_last(&c1).unwrap());
        assert_eq!(out, b"page one of the returnand the final page");
    }

    #[test]
    fn wrong_recovery_phrase_cannot_unwrap_the_source_key() {
        // A DIFFERENT (valid) recovery phrase reconstructs a different master key,
        // which must FAIL to unwrap the source key (AEAD tag mismatch) - so a
        // mistyped-but-checksum-valid phrase can never silently yield garbage.
        let master = MasterKey::generate();
        let (_source_key, wrapped) = master.wrap_new_source_key().unwrap();

        let other_master = MasterKey::generate();
        let other_phrase = master_key_to_phrase(&other_master).unwrap();
        let wrong = phrase_to_master_key(&other_phrase).unwrap();
        assert!(matches!(
            wrong.unwrap_source_key(&wrapped),
            Err(CryptoError::DecryptFailed)
        ));
    }
}
