//! BIP39 recovery phrase over the account master key (DESIGN s7.3).
//!
//! On encryption opt-in the user is shown a 24-word BIP39 phrase encoding
//! the 32-byte [`MasterKey`] and must confirm they stored it. If the OS
//! keychain is later wiped (machine reformat), pasting the phrase back
//! reconstructs the master key, which then unwraps every per-source key.
//!
//! A 256-bit master key maps to exactly 24 BIP39 words (English wordlist).
//! The phrase is the master key bytes plus an 8-bit checksum - no
//! passphrase / PBKDF2 seed stretching is applied: we encode the *entropy*
//! itself (`from_entropy` / `to_entropy`), not a derived seed, so the round
//! trip is exact and lossless.

use bip39::Mnemonic;
use zeroize::Zeroizing;

use crate::key::{MasterKey, KEY_LEN};
use crate::CryptoError;

/// Encodes a master key as its 24-word BIP39 recovery phrase
/// (DESIGN s7.3). The returned [`Zeroizing`] string scrubs the phrase from
/// memory on drop; the UI must render it and then let it drop.
///
/// # Errors
/// Returns [`CryptoError::Protocol`] only if the BIP39 library rejects the
/// 32-byte entropy, which cannot happen for a fixed 256-bit key (the arm
/// exists so no `expect` sits in non-test code).
pub fn master_key_to_phrase(key: &MasterKey) -> Result<Zeroizing<String>, CryptoError> {
    let mnemonic = Mnemonic::from_entropy(key.as_bytes())
        .map_err(|e| CryptoError::Protocol(format!("BIP39 encode failed: {e}")))?;
    Ok(Zeroizing::new(mnemonic.to_string()))
}

/// Reconstructs a master key from a user-supplied BIP39 recovery phrase
/// (DESIGN s7.3). Validates the BIP39 checksum and that the phrase encodes
/// exactly 256 bits.
///
/// # Errors
/// Returns [`CryptoError::RecoveryPhraseInvalid`] if the phrase fails its
/// BIP39 checksum, contains an unknown word, or does not encode a 32-byte
/// key (wrong word count).
pub fn phrase_to_master_key(phrase: &str) -> Result<MasterKey, CryptoError> {
    let mnemonic =
        Mnemonic::parse(phrase.trim()).map_err(|_| CryptoError::RecoveryPhraseInvalid)?;
    let entropy = Zeroizing::new(mnemonic.to_entropy());
    if entropy.len() != KEY_LEN {
        return Err(CryptoError::RecoveryPhraseInvalid);
    }
    let mut bytes = [0u8; KEY_LEN];
    bytes.copy_from_slice(&entropy);
    Ok(MasterKey::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_round_trip() {
        let key = MasterKey::generate();
        let phrase = master_key_to_phrase(&key).unwrap();
        // 256-bit entropy => 24 words.
        assert_eq!(phrase.split_whitespace().count(), 24);
        let recovered = phrase_to_master_key(&phrase).unwrap();
        assert_eq!(key.as_bytes(), recovered.as_bytes());
    }

    #[test]
    fn phrase_is_deterministic() {
        let key = MasterKey::from_bytes([7u8; KEY_LEN]);
        let a = master_key_to_phrase(&key).unwrap();
        let b = master_key_to_phrase(&key).unwrap();
        assert_eq!(*a, *b);
    }

    #[test]
    fn whitespace_tolerated() {
        let key = MasterKey::generate();
        let phrase = master_key_to_phrase(&key).unwrap();
        let padded = format!("  {}  ", *phrase);
        let recovered = phrase_to_master_key(&padded).unwrap();
        assert_eq!(key.as_bytes(), recovered.as_bytes());
    }

    #[test]
    fn bad_checksum_rejected() {
        // Valid words but a deliberately wrong checksum (all "abandon").
        let bad = "abandon abandon abandon abandon abandon abandon abandon abandon \
                   abandon abandon abandon abandon abandon abandon abandon abandon \
                   abandon abandon abandon abandon abandon abandon abandon abandon";
        assert!(matches!(
            phrase_to_master_key(bad),
            Err(CryptoError::RecoveryPhraseInvalid)
        ));
    }

    #[test]
    fn unknown_word_rejected() {
        let bad = "zzzz zzzz zzzz zzzz zzzz zzzz zzzz zzzz \
                   zzzz zzzz zzzz zzzz zzzz zzzz zzzz zzzz \
                   zzzz zzzz zzzz zzzz zzzz zzzz zzzz zzzz";
        assert!(matches!(
            phrase_to_master_key(bad),
            Err(CryptoError::RecoveryPhraseInvalid)
        ));
    }

    #[test]
    fn wrong_word_count_rejected() {
        // A valid 12-word phrase encodes 128 bits, not our 256.
        let twelve = "legal winner thank year wave sausage worth useful legal winner thank yellow";
        assert!(matches!(
            phrase_to_master_key(twelve),
            Err(CryptoError::RecoveryPhraseInvalid)
        ));
    }
}
