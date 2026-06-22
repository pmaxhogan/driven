//! `driven-crypto` — Driven's authenticated-encryption format.
//!
//! Owns master/per-source key types, OS-keychain key storage,
//! per-path filename encryption (XChaCha20-Poly1305 + base32),
//! chunked content encryption (XChaCha20-Poly1305 STREAM via
//! `chacha20poly1305::aead::stream::EncryptorBE32`), and BIP39
//! recovery-phrase encoding of the master key.
