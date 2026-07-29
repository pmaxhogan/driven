//! `driven-rclone` - read an existing `rclone.conf` and translate its remotes
//! into Driven destination settings.
//!
//! Someone arriving from rclone already has every endpoint, region and access
//! key typed out once. This crate reads that file and says, per remote, exactly
//! what Driven would use - so the migration is a confirmation rather than a
//! transcription exercise. It is driven by `driven-cli rclone list` /
//! `driven-cli rclone import`.
//!
//! ## Shape
//!
//! - [`config_file`] - the INI-ish parser, the default-location search path, and
//!   whole-file-encryption detection.
//! - [`import`] - [`classify`], which maps one remote onto an S3 destination, a
//!   Drive destination, or an explained refusal.
//! - [`render`] - the human and JSON renderings the CLI prints.
//! - [`secret`] - [`Secret`], the wrapper that keeps a credential out of `Debug`,
//!   `Display`, logs and error messages.
//!
//! ## What this crate deliberately does NOT do
//!
//! **It never writes anything.** No keychain entry, no `accounts` row, no file.
//! Creating a Driven destination means probing the endpoint with the credentials
//! before persisting, rolling the keychain entry back if the row write fails,
//! and hot-spawning the account's orchestrator - all of which lives in
//! `create_s3_account`. An importer that inserted rows directly would skip every
//! one of those steps and, with the desktop app running, write behind the back
//! of an already-loaded account cache. So the importer TRANSLATES and the
//! existing creation path still CREATES.
//!
//! ## Credentials
//!
//! Two rules, both enforced by tests rather than by convention:
//!
//! 1. Any value that could authenticate someone is a [`Secret`], which renders
//!    as `<redacted>` through `Debug` and `Display`. Reaching the value means
//!    calling `Secret::expose`, which is greppable.
//! 2. No parse error, note or blocker message ever contains a config VALUE.
//!    Errors carry a line number and an option name.
//!
//! The rendered output prints a secret only when the caller explicitly asks
//! ([`render::RenderOptions::reveal_secrets`]), and the CLI gates that behind a
//! flag whose help text says it prints a credential to stdout.
//!
//! ## Obscured values
//!
//! rclone can store an option "obscured" (AES-256-CTR under a hard-coded key -
//! obfuscation, not encryption). **Neither importable remote type has one.**
//! Obscuring is applied only to options a backend declares with
//! `IsPassword: true`; `backend/s3/s3.go` declares none at all
//! (`secret_access_key` is `Sensitive: true`, which only affects redaction in
//! rclone's own output, not how it is stored), and `backend/drive/drive.go` has
//! no password option either. Obscured values belong to `crypt`'s
//! `password`/`password2`, `sftp`'s `pass`, and similar - all of which are
//! remote types Driven cannot target anyway.
//!
//! So this crate ships no cipher and no `reveal`. Adding AES to decode a value
//! that cannot appear in an imported remote would be untested-in-anger code
//! sitting on the credential path, which is the wrong trade.
//!
//! ## Encrypted configs
//!
//! A whole-`rclone.conf` password-encrypted file is DETECTED and refused with
//! instructions, never decrypted. See
//! [`config_file::ConfigParseError::Encrypted`] for the reasoning.

#![deny(missing_docs)]

pub mod config_file;
pub mod import;
pub mod render;
pub mod secret;

pub use config_file::{
    config_search_path, find_config_file, locate_config, ConfigCandidate, ConfigParseError,
    EnvLookup, ProcessEnv, RcloneConfig, RcloneRemote,
};
pub use import::{classify, DriveRemote, RemoteImport, S3Remote, UnsupportedReason};
pub use render::{render_detail, render_list, to_json, to_json_pretty, RenderOptions};
pub use secret::Secret;

/// Classify every remote in a parsed config, in file order.
pub fn classify_all(config: &RcloneConfig) -> Vec<RemoteImport> {
    config.remotes().iter().map(classify).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_all_preserves_file_order_and_covers_every_section() {
        let cfg =
            RcloneConfig::parse("[one]\ntype = s3\n[two]\ntype = drive\n[three]\ntype = sftp\n")
                .unwrap();
        let out = classify_all(&cfg);
        assert_eq!(
            out.iter()
                .map(RemoteImport::remote_name)
                .collect::<Vec<_>>(),
            vec!["one", "two", "three"]
        );
        assert!(matches!(out[0], RemoteImport::S3(_)));
        assert!(matches!(out[1], RemoteImport::Drive(_)));
        assert!(matches!(out[2], RemoteImport::Unsupported { .. }));
    }

    #[test]
    fn classify_all_of_an_empty_config_is_empty() {
        assert!(classify_all(&RcloneConfig::parse("# nothing here\n").unwrap()).is_empty());
    }
}
