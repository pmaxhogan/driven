//! Updater signature smoke (M9 R5-P1-4, GA-blocker regression guard).
//!
//! Codex M9-5 claimed the `tauri.conf.json` updater `pubkey` (base64 of the
//! whole minisign `.pub` file) was wrong and that Tauri wants the bare `RWS...`
//! line instead - a change that, if applied, would BREAK production update
//! verification. This test settles the format EMPIRICALLY and then locks it in.
//!
//! THE EMPIRICAL VERDICT (codex was WRONG): tauri-plugin-updater 2.10.1's
//! `verify_signature` (updater.rs:1453) does:
//!
//! ```ignore
//! let pub_key_decoded = base64_to_string(pub_key)?;     // base64-DECODE the config pubkey
//! let public_key = PublicKey::decode(&pub_key_decoded)?; // THEN parse the minisign text
//! let signature_base64_decoded = base64_to_string(release_signature)?;
//! let signature = Signature::decode(&signature_base64_decoded)?;
//! public_key.verify(data, &signature, true)?;            // allow_legacy = true
//! ```
//!
//! i.e. the updater base64-DECODES the configured `pubkey` first, so the config
//! MUST be the base64 of the entire `.pub` file (whose decoded content is
//! `untrusted comment: ...\nRWS...`). The bare `RWS...` line would FAIL
//! `base64_to_string` / `PublicKey::decode` and break every update. So the
//! configured value is correct and is KEPT.
//!
//! This test replicates that exact decode+verify path against:
//!   - the `pubkey` read live from `tauri.conf.json` (so a wrong pubkey edit, or
//!     key drift, fails CI here before a release is cut), and
//!   - a committed fixture (`tests/fixtures/updater/fixture.bin`) + its
//!     committed signature (`fixture.bin.sig`, produced once by
//!     `cargo tauri signer sign` with the Driven updater private key).
//!
//! VERIFY needs only the public key + the committed fixture + sig - NO private
//! key - so CI runs this on every build with nothing secret checked in.

use base64::Engine;
use minisign_verify::{PublicKey, Signature};

/// The fixture artifact + its detached signature, committed alongside the test.
const FIXTURE_BIN: &[u8] = include_bytes!("fixtures/updater/fixture.bin");
/// The `.sig` is itself base64 (the tauri signer output / what an update server
/// stores in the `signature` field). `include_str!` keeps the trailing newline
/// the signer wrote; `str::trim` strips it before the base64 decode.
const FIXTURE_SIG_B64: &str = include_str!("fixtures/updater/fixture.bin.sig");

/// Read the configured updater `pubkey` straight out of `tauri.conf.json` so the
/// test guards the ACTUAL deployed value (not a copy). Parses the JSON and pulls
/// `plugins.updater.pubkey`.
fn configured_pubkey() -> String {
    let conf_path = concat!(env!("CARGO_MANIFEST_DIR"), "/tauri.conf.json");
    let raw = std::fs::read_to_string(conf_path)
        .unwrap_or_else(|e| panic!("read tauri.conf.json ({conf_path}): {e}"));
    let json: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse tauri.conf.json: {e}"));
    json.get("plugins")
        .and_then(|p| p.get("updater"))
        .and_then(|u| u.get("pubkey"))
        .and_then(|k| k.as_str())
        .map(str::to_string)
        .expect("tauri.conf.json plugins.updater.pubkey must be a string")
}

/// Replicate tauri-plugin-updater 2.10.1 `base64_to_string`: STANDARD base64
/// decode, then interpret the bytes as UTF-8.
fn base64_to_string(s: &str) -> Result<String, String> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|e| format!("base64 decode: {e}"))?;
    String::from_utf8(decoded).map_err(|e| format!("utf8: {e}"))
}

/// Replicate the EXACT verify path tauri-plugin-updater uses (updater.rs:1453):
/// base64-decode the pubkey -> `PublicKey::decode`; base64-decode the sig ->
/// `Signature::decode`; `verify(data, &sig, allow_legacy = true)`.
fn verify_like_tauri(data: &[u8], sig_b64: &str, pubkey_config: &str) -> Result<(), String> {
    let pub_key_decoded = base64_to_string(pubkey_config)?;
    let public_key =
        PublicKey::decode(&pub_key_decoded).map_err(|e| format!("PublicKey::decode: {e}"))?;
    let sig_decoded = base64_to_string(sig_b64)?;
    let signature =
        Signature::decode(&sig_decoded).map_err(|e| format!("Signature::decode: {e}"))?;
    // `true` == allow_legacy, matching tauri's `public_key.verify(data, &sig, true)`.
    public_key
        .verify(data, &signature, true)
        .map_err(|e| format!("verify: {e}"))
}

#[test]
fn configured_pubkey_verifies_the_committed_fixture_signature() {
    // GA-blocker regression guard: the committed signature MUST verify against
    // the pubkey configured in tauri.conf.json, via tauri's own decode path. If
    // this fails, signed production updates will fail verification.
    let pubkey = configured_pubkey();
    verify_like_tauri(FIXTURE_BIN, FIXTURE_SIG_B64, &pubkey).unwrap_or_else(|e| {
        panic!(
            "configured tauri.conf.json updater pubkey did NOT verify the committed \
             fixture signature ({e}). The pubkey must be the base64 of the whole \
             minisign .pub file (decoded content `untrusted comment: ...\\nRWS...`), \
             NOT the bare RWS line. Do not change it without re-signing the fixture."
        )
    });
}

#[test]
fn configured_pubkey_is_base64_of_a_minisign_pub_file_not_the_bare_rws_line() {
    // Lock in the FORMAT verdict: the configured value base64-decodes to the
    // minisign .pub text (an `untrusted comment:` header + an `RWS...` line). The
    // bare `RWS...` line - codex's proposed "fix" - would NOT base64-decode to
    // valid UTF-8 minisign text and `PublicKey::decode` would reject it.
    let pubkey = configured_pubkey();
    let decoded = base64_to_string(&pubkey)
        .expect("configured pubkey must be valid STANDARD base64 of UTF-8 text");
    assert!(
        decoded.contains("untrusted comment:") && decoded.contains("RWS"),
        "decoded pubkey must be minisign .pub text (untrusted comment + RWS line): {decoded:?}"
    );
    // And it must actually parse as a minisign public key.
    PublicKey::decode(&decoded).expect("decoded pubkey must parse as a minisign PublicKey");

    // The bare RWS line (codex's claim) must NOT be accepted as the config value:
    // it is not base64 of UTF-8 minisign text, so tauri's decode path rejects it.
    let bare_rws = decoded
        .lines()
        .find(|l| l.starts_with("RWS"))
        .expect("decoded pubkey has an RWS line");
    assert!(
        verify_like_tauri(FIXTURE_BIN, FIXTURE_SIG_B64, bare_rws).is_err(),
        "the bare RWS line must FAIL tauri's verify path (proves the config must be \
         the base64-of-.pub-file form, not the RWS line)"
    );
}

#[test]
fn tampered_fixture_fails_verification() {
    // Prove the smoke actually checks the signature (not a no-op): flipping one
    // byte of the signed data must make verification fail.
    let pubkey = configured_pubkey();
    let mut tampered = FIXTURE_BIN.to_vec();
    tampered[0] ^= 0xFF;
    assert!(
        verify_like_tauri(&tampered, FIXTURE_SIG_B64, &pubkey).is_err(),
        "tampered fixture must NOT verify"
    );
}
