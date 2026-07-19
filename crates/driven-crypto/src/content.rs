//! Chunked content encryption via the XChaCha20-Poly1305 STREAM
//! construction (DESIGN s7.1).
//!
//! Each file gets a fresh random 24-byte nonce written into a 40-byte
//! header (`16-byte magic || 24-byte nonce`). The
//! [`chacha20poly1305::aead::stream`] BE32 construction adds a 5-byte
//! overhead (4-byte big-endian chunk counter + 1-byte last-block flag) to
//! the AEAD nonce, so the STREAM primitive itself takes the **first 19
//! bytes** of the per-file nonce - the remaining 5 bytes are reserved by
//! the counter/flag. All 24 bytes still travel in the header so the format
//! is self-describing for `driven-cli decrypt` (DESIGN s7).
//!
//! Drive computes its `md5Checksum` over the bytes it receives, which are
//! the ciphertext bytes (header + every chunk). So the encryptor threads a
//! single MD5 hasher through `header()`, `encrypt_chunk()`, and
//! `finalize_last()` and returns that digest for the verify step
//! (DESIGN s7.1 "Drive's md5Checksum is the ciphertext md5").

use bytes::Bytes;
use chacha20poly1305::{
    aead::{
        generic_array::GenericArray,
        stream::{DecryptorBE32, EncryptorBE32},
    },
    KeyInit, XChaCha20Poly1305,
};
use md5::{Digest, Md5};
use rand::TryRng;

use crate::key::{SourceKey, XNONCE_LEN};
use crate::CryptoError;

/// 16-byte file-header magic identifying Driven content ciphertext
/// (`b"DRIVENc1" || b"DRIVENc1"` => `DRIVENc1DRIVENc1`, ASCII, 16 bytes).
/// Bumping the trailing version digit signals a format change.
pub const CONTENT_MAGIC: [u8; 16] = *b"DRIVENc1DRIVENc1";

/// Total length of the content file header: 16-byte magic + 24-byte nonce.
pub const HEADER_LEN: usize = 16 + XNONCE_LEN;

/// Number of nonce bytes the BE32 STREAM primitive consumes: the
/// XChaCha20-Poly1305 nonce (24) minus the 5-byte BE32 overhead.
const STREAM_NONCE_LEN: usize = XNONCE_LEN - 5;

/// Plaintext chunk size fed to [`ContentEncryptorImpl::encrypt_chunk`]
/// (DESIGN s7.1: 64 KiB plaintext -> 64 KiB + 16-byte tag ciphertext). The
/// executor is responsible for chunking at this boundary; the crypto layer
/// does not require it but documents the design intent.
pub const PLAINTEXT_CHUNK_LEN: usize = 64 * 1024;

/// Concrete [`crate::ContentEncryptor`] over XChaCha20-Poly1305 STREAM.
pub struct ContentEncryptorImpl {
    /// `Some` until consumed; the BE32 STREAM encryptor advances its counter
    /// internally on each `encrypt_next`.
    inner: Option<EncryptorBE32<XChaCha20Poly1305>>,
    /// The per-file nonce (all 24 bytes) for the header.
    nonce: [u8; XNONCE_LEN],
    /// MD5 over every ciphertext byte emitted (header + chunks).
    md5: Md5,
    /// Guards the `header()`-before-chunks and single-`header()` contract.
    header_emitted: bool,
}

impl ContentEncryptorImpl {
    /// Builds an encryptor for one file with a freshly generated nonce.
    pub(crate) fn new(source_key: &SourceKey) -> Self {
        let mut nonce = [0u8; XNONCE_LEN];
        // `SysRng` (rand 0.10's OS RNG, re-exported from `getrandom`) is
        // fallible (`TryRng`, not the infallible `Rng`) since reading OS
        // entropy can fail. `.expect()` preserves rand 0.8's `OsRng::fill_bytes`
        // behavior, which panicked internally on the same failure.
        rand::rngs::SysRng
            .try_fill_bytes(&mut nonce)
            .expect("OS RNG (getrandom) failed to fill content nonce");
        let cipher = XChaCha20Poly1305::new(source_key.as_bytes().as_ref().into());
        let stream_nonce = GenericArray::from_slice(&nonce[..STREAM_NONCE_LEN]);
        Self {
            inner: Some(EncryptorBE32::from_aead(cipher, stream_nonce)),
            nonce,
            md5: Md5::new(),
            header_emitted: false,
        }
    }
}

impl crate::ContentEncryptor for ContentEncryptorImpl {
    fn header(&mut self) -> Bytes {
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(&CONTENT_MAGIC);
        header.extend_from_slice(&self.nonce);
        self.md5.update(&header);
        self.header_emitted = true;
        Bytes::from(header)
    }

    fn encrypt_chunk(&mut self, plaintext: &[u8]) -> Result<Bytes, CryptoError> {
        if !self.header_emitted {
            return Err(CryptoError::Protocol(
                "encrypt_chunk called before header".to_string(),
            ));
        }
        let stream = self
            .inner
            .as_mut()
            .ok_or_else(|| CryptoError::Protocol("encrypt_chunk after finalize".to_string()))?;
        let ciphertext = stream
            .encrypt_next(plaintext)
            .map_err(|_| CryptoError::Protocol("STREAM encrypt_next failed".to_string()))?;
        self.md5.update(&ciphertext);
        Ok(Bytes::from(ciphertext))
    }

    fn finalize_last(
        mut self: Box<Self>,
        plaintext: &[u8],
    ) -> Result<(Bytes, [u8; 16]), CryptoError> {
        if !self.header_emitted {
            return Err(CryptoError::Protocol(
                "finalize_last called before header".to_string(),
            ));
        }
        let stream = self
            .inner
            .take()
            .ok_or_else(|| CryptoError::Protocol("finalize_last after finalize".to_string()))?;
        let ciphertext = stream
            .encrypt_last(plaintext)
            .map_err(|_| CryptoError::Protocol("STREAM encrypt_last failed".to_string()))?;
        self.md5.update(&ciphertext);
        let digest: [u8; 16] = self.md5.finalize().into();
        Ok((Bytes::from(ciphertext), digest))
    }
}

/// Concrete [`crate::ContentDecryptor`] over XChaCha20-Poly1305 STREAM.
pub struct ContentDecryptorImpl {
    inner: Option<DecryptorBE32<XChaCha20Poly1305>>,
}

impl ContentDecryptorImpl {
    /// Builds a decryptor from the 40-byte header read off the Drive object.
    ///
    /// # Errors
    /// Returns [`CryptoError::Protocol`] if the header is the wrong length
    /// or its magic does not match [`CONTENT_MAGIC`].
    pub(crate) fn from_header(source_key: &SourceKey, header: &[u8]) -> Result<Self, CryptoError> {
        if header.len() != HEADER_LEN {
            return Err(CryptoError::Protocol(format!(
                "content header must be {HEADER_LEN} bytes, got {}",
                header.len()
            )));
        }
        if header[..16] != CONTENT_MAGIC {
            return Err(CryptoError::Protocol(
                "content header magic mismatch".to_string(),
            ));
        }
        let cipher = XChaCha20Poly1305::new(source_key.as_bytes().as_ref().into());
        let stream_nonce = GenericArray::from_slice(&header[16..16 + STREAM_NONCE_LEN]);
        Ok(Self {
            inner: Some(DecryptorBE32::from_aead(cipher, stream_nonce)),
        })
    }
}

impl crate::ContentDecryptor for ContentDecryptorImpl {
    fn decrypt_chunk(&mut self, ciphertext: &[u8]) -> Result<Bytes, CryptoError> {
        let stream = self
            .inner
            .as_mut()
            .ok_or_else(|| CryptoError::Protocol("decrypt_chunk after finalize".to_string()))?;
        let plaintext = stream
            .decrypt_next(ciphertext)
            .map_err(|_| CryptoError::DecryptFailed)?;
        Ok(Bytes::from(plaintext))
    }

    fn decrypt_last(mut self: Box<Self>, ciphertext: &[u8]) -> Result<Bytes, CryptoError> {
        let stream = self
            .inner
            .take()
            .ok_or_else(|| CryptoError::Protocol("decrypt_last after finalize".to_string()))?;
        let plaintext = stream
            .decrypt_last(ciphertext)
            .map_err(|_| CryptoError::DecryptFailed)?;
        Ok(Bytes::from(plaintext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentDecryptor, ContentEncryptor};

    fn round_trip(chunks: &[&[u8]]) -> Vec<u8> {
        let key = SourceKey::generate();
        let mut enc: Box<dyn ContentEncryptor> = Box::new(ContentEncryptorImpl::new(&key));
        let header = enc.header();
        let mut cipher_chunks: Vec<Bytes> = Vec::new();
        for w in &chunks[..chunks.len() - 1] {
            cipher_chunks.push(enc.encrypt_chunk(w).unwrap());
        }
        let (last_ct, _md5) = enc.finalize_last(chunks[chunks.len() - 1]).unwrap();

        let mut dec: Box<dyn ContentDecryptor> =
            Box::new(ContentDecryptorImpl::from_header(&key, &header).unwrap());
        let mut out = Vec::new();
        for ct in &cipher_chunks {
            out.extend_from_slice(&dec.decrypt_chunk(ct).unwrap());
        }
        out.extend_from_slice(&dec.decrypt_last(&last_ct).unwrap());
        out
    }

    #[test]
    fn single_chunk_round_trip() {
        let pt: &[u8] = b"hello driven";
        assert_eq!(round_trip(&[pt]), pt);
    }

    #[test]
    fn multi_chunk_round_trip() {
        let a = vec![0xABu8; PLAINTEXT_CHUNK_LEN];
        let b = vec![0xCDu8; PLAINTEXT_CHUNK_LEN];
        let c = vec![0xEFu8; 1234];
        let mut expected = Vec::new();
        expected.extend_from_slice(&a);
        expected.extend_from_slice(&b);
        expected.extend_from_slice(&c);
        assert_eq!(round_trip(&[&a, &b, &c]), expected);
    }

    #[test]
    fn empty_file_round_trip() {
        assert_eq!(round_trip(&[b""]), b"");
    }

    #[test]
    fn md5_covers_header_and_chunks() {
        let key = SourceKey::generate();
        let mut enc: Box<dyn ContentEncryptor> = Box::new(ContentEncryptorImpl::new(&key));
        let header = enc.header();
        let (last_ct, md5) = enc.finalize_last(b"payload").unwrap();
        // Independently MD5 header || ciphertext and compare.
        let mut h = Md5::new();
        h.update(&header);
        h.update(&last_ct);
        let expected: [u8; 16] = h.finalize().into();
        assert_eq!(md5, expected);
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = SourceKey::generate();
        let mut enc: Box<dyn ContentEncryptor> = Box::new(ContentEncryptorImpl::new(&key));
        let header = enc.header();
        let (mut last_ct, _md5) = enc.finalize_last(b"secret data here").unwrap();
        let mut tampered = last_ct.to_vec();
        tampered[0] ^= 0xff;
        last_ct = Bytes::from(tampered);

        let dec: Box<dyn ContentDecryptor> =
            Box::new(ContentDecryptorImpl::from_header(&key, &header).unwrap());
        assert!(matches!(
            dec.decrypt_last(&last_ct),
            Err(CryptoError::DecryptFailed)
        ));
    }

    #[test]
    fn wrong_key_fails() {
        let key = SourceKey::generate();
        let other = SourceKey::generate();
        let mut enc: Box<dyn ContentEncryptor> = Box::new(ContentEncryptorImpl::new(&key));
        let header = enc.header();
        let (last_ct, _md5) = enc.finalize_last(b"data").unwrap();
        let dec: Box<dyn ContentDecryptor> =
            Box::new(ContentDecryptorImpl::from_header(&other, &header).unwrap());
        assert!(matches!(
            dec.decrypt_last(&last_ct),
            Err(CryptoError::DecryptFailed)
        ));
    }

    #[test]
    fn chunk_reorder_fails() {
        let key = SourceKey::generate();
        let mut enc: Box<dyn ContentEncryptor> = Box::new(ContentEncryptorImpl::new(&key));
        let header = enc.header();
        let c0 = enc.encrypt_chunk(b"first chunk").unwrap();
        let (c1, _md5) = enc.finalize_last(b"second chunk").unwrap();

        let mut dec: Box<dyn ContentDecryptor> =
            Box::new(ContentDecryptorImpl::from_header(&key, &header).unwrap());
        // Feed the second chunk first - STREAM counter mismatch => failure.
        assert!(matches!(
            dec.decrypt_chunk(&c1),
            Err(CryptoError::DecryptFailed)
        ));
        let _ = c0;
    }

    #[test]
    fn header_is_forty_bytes() {
        let key = SourceKey::generate();
        let mut enc: Box<dyn ContentEncryptor> = Box::new(ContentEncryptorImpl::new(&key));
        assert_eq!(enc.header().len(), HEADER_LEN);
        assert_eq!(HEADER_LEN, 40);
    }

    #[test]
    fn bad_header_rejected() {
        let key = SourceKey::generate();
        assert!(matches!(
            ContentDecryptorImpl::from_header(&key, &[0u8; 10]),
            Err(CryptoError::Protocol(_))
        ));
        assert!(matches!(
            ContentDecryptorImpl::from_header(&key, &[0u8; HEADER_LEN]),
            Err(CryptoError::Protocol(_))
        ));
    }
}
