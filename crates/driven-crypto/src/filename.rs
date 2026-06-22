//! Per-path-component filename encryption (DESIGN s7.1).
//!
//! Each path component is encrypted independently with XChaCha20-Poly1305
//! using two BLAKE3-derived sub-keys off the per-source key (distinct
//! domain-separation contexts so the same bytes are never reused in two
//! roles):
//!
//! - `nonce_key = blake3::derive_key("driven-filename-nonce-v1", per_source_key)`
//! - `aead_key  = blake3::derive_key("driven-filename-aead-v1",  per_source_key)`
//!
//! The 24-byte nonce is a keyed BLAKE3 hash over `parent_aad || 0xff ||
//! component` (the `0xff` separator avoids concatenation ambiguity),
//! truncated to 24 bytes. It is **deterministic**, so the same component in
//! the same folder always maps to the same ciphertext name, which the
//! executor's `files.list` lookups require. A deterministic nonce is safe
//! because the 192-bit nonce space plus the derived per-source `aead_key`
//! make accidental keystream reuse infeasible, and a given nonce only ever
//! meets one plaintext (DESIGN s7.1 corrected note). Equality detection is
//! a desired property for filenames, not a leak to hide.
//!
//! ## On-disk name layout (deviation from DESIGN's literal length formula)
//!
//! The frozen [`crate::SourceCryptoSuite::decrypt_filename`] signature
//! receives only the ciphertext name and the parent's AAD - never this
//! component's plaintext. Since the nonce is a function of the (unknown at
//! decrypt time) plaintext, it cannot be recomputed during decrypt, so it
//! must be carried in the name. The encoded name is therefore
//! `base32hex(nonce[24] || aead_ciphertext)` (the SIV-style deterministic
//! AEAD pattern). Determinism is preserved end to end: deterministic nonce
//! plus deterministic ciphertext yields a stable name. NOTE: this makes the
//! encoded length `ceil((N + 16 + 24) times 8/5)`, i.e. 24 plaintext bytes
//! more than DESIGN s7.1's `ceil((N + 16) times 8/5)` formula, which omitted
//! the stored nonce.
//!
//! ## Nonce input includes the parent AAD (faithful to DESIGN's path intent)
//!
//! DESIGN s7.1 derives the nonce from the canonical *path-from-source-root*.
//! Folding `parent_ciphertext_aad` into the nonce derivation keeps that
//! intent and avoids a cross-folder filename-equality leak (a component
//! named `passwords.txt` encrypts differently under different parents). The
//! parent is also bound as the AEAD AAD so moving a folder re-encrypts its
//! children (DESIGN s7.1). The executor canonicalises `component` (NFC +
//! case-fold) before calling, per the trait contract, so this module does
//! not re-normalise.
//!
//! Ciphertext is encoded base32hex (RFC 4648 s7, lowercase, no padding) so
//! it is a valid Drive filename on every platform.

use base32::Alphabet;
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};
use zeroize::Zeroizing;

use crate::key::SourceKey;
use crate::CryptoError;

/// BLAKE3 derive-key context for the filename nonce sub-key.
const NONCE_CONTEXT: &str = "driven-filename-nonce-v1";
/// BLAKE3 derive-key context for the filename AEAD sub-key.
const AEAD_CONTEXT: &str = "driven-filename-aead-v1";

/// Length of the XChaCha20-Poly1305 nonce prepended to each ciphertext name.
const NONCE_LEN: usize = 24;

/// Poly1305 tag length (minimum AEAD ciphertext for an empty plaintext).
const TAG_LEN: usize = 16;

/// base32hex without padding: lowercase output, case-insensitive decode
/// (RFC 4648 s7).
const FILENAME_ALPHABET: Alphabet = Alphabet::Rfc4648HexLower { padding: false };

/// Holds the two filename sub-keys derived from a per-source key. Both are
/// zeroized on drop via [`Zeroizing`].
pub(crate) struct FilenameKeys {
    nonce_key: Zeroizing<[u8; 32]>,
    aead_key: Zeroizing<[u8; 32]>,
}

impl FilenameKeys {
    /// Derives the nonce and AEAD sub-keys from the per-source key
    /// (DESIGN s7.1).
    pub(crate) fn derive(source_key: &SourceKey) -> Self {
        let nonce_key = Zeroizing::new(blake3::derive_key(NONCE_CONTEXT, source_key.as_bytes()));
        let aead_key = Zeroizing::new(blake3::derive_key(AEAD_CONTEXT, source_key.as_bytes()));
        Self {
            nonce_key,
            aead_key,
        }
    }

    /// Deterministic 24-byte nonce for `(component, parent_aad)`:
    /// `keyed_hash(nonce_key, parent_aad || 0xff || component)` truncated.
    fn nonce_for(&self, component: &str, parent_ciphertext_aad: &[u8]) -> [u8; NONCE_LEN] {
        let mut hasher = blake3::Hasher::new_keyed(&self.nonce_key);
        hasher.update(parent_ciphertext_aad);
        // Separator byte: 0xff is not a valid leading byte of any UTF-8
        // continuation, so `parent || 0xff || component` is unambiguous.
        hasher.update(&[0xff]);
        hasher.update(component.as_bytes());
        let hash = hasher.finalize();
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&hash.as_bytes()[..NONCE_LEN]);
        nonce
    }

    /// Encrypts one already-canonical path component, returning its
    /// base32hex ciphertext name `base32hex(nonce || ciphertext)`
    /// (DESIGN s7.1).
    pub(crate) fn encrypt(
        &self,
        component: &str,
        parent_ciphertext_aad: &[u8],
    ) -> Result<String, CryptoError> {
        let cipher = XChaCha20Poly1305::new(self.aead_key.as_slice().into());
        let nonce_bytes = self.nonce_for(component, parent_ciphertext_aad);
        let nonce = XNonce::from_slice(&nonce_bytes);
        let ciphertext = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: component.as_bytes(),
                    aad: parent_ciphertext_aad,
                },
            )
            .map_err(|_| CryptoError::Protocol("filename encrypt failed".to_string()))?;
        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        Ok(base32::encode(FILENAME_ALPHABET, &blob))
    }

    /// Inverse of [`Self::encrypt`]: decodes the base32hex name, splits off
    /// the prepended nonce, and AEAD-decrypts back to the plaintext
    /// component (DESIGN s7.3).
    pub(crate) fn decrypt(
        &self,
        ciphertext_name: &str,
        parent_ciphertext_aad: &[u8],
    ) -> Result<String, CryptoError> {
        let blob = base32::decode(FILENAME_ALPHABET, ciphertext_name)
            .ok_or_else(|| CryptoError::Protocol("filename base32hex decode failed".to_string()))?;
        if blob.len() < NONCE_LEN + TAG_LEN {
            return Err(CryptoError::Protocol(
                "filename ciphertext too short".to_string(),
            ));
        }
        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
        let cipher = XChaCha20Poly1305::new(self.aead_key.as_slice().into());
        let nonce = XNonce::from_slice(nonce_bytes);
        let plaintext = cipher
            .decrypt(
                nonce,
                Payload {
                    msg: ciphertext,
                    aad: parent_ciphertext_aad,
                },
            )
            .map_err(|_| CryptoError::DecryptFailed)?;
        String::from_utf8(plaintext)
            .map_err(|_| CryptoError::Protocol("decrypted filename not UTF-8".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> FilenameKeys {
        FilenameKeys::derive(&SourceKey::generate())
    }

    #[test]
    fn round_trip_top_level() {
        let k = keys();
        let ct = k.encrypt("Documents", &[]).unwrap();
        assert_eq!(k.decrypt(&ct, &[]).unwrap(), "Documents");
    }

    #[test]
    fn round_trip_with_parent_aad() {
        let k = keys();
        let parent = k.encrypt("photos", &[]).unwrap();
        let ct = k.encrypt("vacation.jpg", parent.as_bytes()).unwrap();
        assert_eq!(k.decrypt(&ct, parent.as_bytes()).unwrap(), "vacation.jpg");
    }

    #[test]
    fn deterministic_same_inputs() {
        let k = keys();
        let a = k.encrypt("report.pdf", b"parent").unwrap();
        let b = k.encrypt("report.pdf", b"parent").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn different_parent_gives_different_name() {
        let k = keys();
        let in_a = k.encrypt("passwords.txt", b"folderA").unwrap();
        let in_b = k.encrypt("passwords.txt", b"folderB").unwrap();
        assert_ne!(in_a, in_b);
    }

    #[test]
    fn wrong_parent_aad_fails() {
        let k = keys();
        let ct = k.encrypt("secret.txt", b"realparent").unwrap();
        assert!(matches!(
            k.decrypt(&ct, b"wrongparent"),
            Err(CryptoError::DecryptFailed)
        ));
    }

    #[test]
    fn tampered_name_fails() {
        let k = keys();
        let ct = k.encrypt("data.bin", &[]).unwrap();
        // Mutate the FIRST char (top 5 bits of nonce byte 0 - all real bits,
        // never base32 padding). Flipping a trailing char could only touch a
        // discarded pad bit and decode to the same bytes; char 0 guarantees a
        // genuine ciphertext/nonce change, so the AEAD tag fails
        // deterministically.
        let mut chars: Vec<char> = ct.chars().collect();
        chars[0] = if chars[0] == 'a' { 'b' } else { 'a' };
        let tampered: String = chars.into_iter().collect();
        assert!(matches!(
            k.decrypt(&tampered, &[]),
            Err(CryptoError::DecryptFailed) | Err(CryptoError::Protocol(_))
        ));
    }

    #[test]
    fn cross_source_uncorrelated() {
        let k1 = keys();
        let k2 = keys();
        let a = k1.encrypt("same.txt", &[]).unwrap();
        let b = k2.encrypt("same.txt", &[]).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn unicode_component_round_trip() {
        let k = keys();
        let name = "café-naïve.txt";
        let ct = k.encrypt(name, &[]).unwrap();
        assert_eq!(k.decrypt(&ct, &[]).unwrap(), name);
    }

    #[test]
    fn short_blob_rejected() {
        let k = keys();
        assert!(matches!(
            k.decrypt("00", &[]),
            Err(CryptoError::Protocol(_))
        ));
    }
}
