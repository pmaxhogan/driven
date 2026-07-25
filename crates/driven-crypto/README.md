# `driven-crypto`

Driven's authenticated-encryption format: nothing leaves the machine unencrypted,
and this crate owns every byte of that. Specified in `design/DESIGN.md` s7.

- `content.rs` - chunked content encryption (XChaCha20-Poly1305 STREAM) and the
  ciphertext MD5 that Drive verifies the upload against
- `filename.rs` - per path-component filename encryption with base32hex encoding,
  using the parent component's ciphertext as AEAD AAD
- `key.rs` / `keystore.rs` - master and per-source keys, OS-keychain storage
- `recovery.rs` - BIP39 encoding of the master key (the user's recovery phrase)

The `SourceCryptoSuite` trait in `lib.rs` is the seam `driven-core`'s executor codes
against. It deliberately references no `driven-core` type - every signature is
`&str` / `Bytes` / `[u8; 16]` - so the dependency graph stays one-way. This crate is
a leaf. Read the `lib.rs` module doc before touching the seam; it explains why
content encryption yields a per-file encryptor object rather than a one-shot call.

```sh
cargo test -p driven-crypto
```
