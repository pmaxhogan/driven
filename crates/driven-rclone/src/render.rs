//! Turning a classified remote into what the user reads.
//!
//! Rendering lives here rather than in `driven-cli` so every line of output is
//! unit-tested - including the ones that must NOT appear. The CLI is a
//! `println!` of what these functions return.
//!
//! Two renderings, from the same data:
//!
//! - [`render_list`] - one line per remote, for "what have I got".
//! - [`render_detail`] / [`to_json`] - everything Driven would use for one
//!   remote, plus the notes and blockers.

use serde_json::{json, Map, Value};

use crate::import::{DriveRemote, RemoteImport, S3Remote};
use crate::secret::Secret;

/// How much to render, and whether credentials may be printed.
#[derive(Debug, Clone, Default)]
pub struct RenderOptions {
    /// The bucket the user named. rclone remotes carry no bucket (it is part of
    /// the path you type), so without this an S3 rendering shows the setting as
    /// outstanding rather than guessing.
    pub bucket: Option<String>,
    /// An optional key prefix to confine Driven to a subtree.
    pub prefix: Option<String>,
    /// Print secret values instead of `<redacted>`.
    ///
    /// OFF by default, and the CLI flag that turns it on says in its help that
    /// it writes a credential to stdout. Everything else about a remote is
    /// printable without it.
    ///
    /// The default is only usable because the redacted rendering says WHERE the
    /// value is (see [`Self::config_path`]) - "hidden" without "and here is how
    /// to get it" would just be a dead end that forces everyone to pass the
    /// flag.
    pub reveal_secrets: bool,
    /// The config file being read, so the redacted rendering can point at the
    /// exact line to copy from instead of leaving the user to hunt for it.
    pub config_path: Option<String>,
}

impl RenderOptions {
    /// Render `secret` according to [`Self::reveal_secrets`].
    fn show(&self, secret: &Secret) -> String {
        if self.reveal_secrets {
            secret.expose().to_string()
        } else {
            crate::secret::REDACTED.to_string()
        }
    }
}

/// One line per remote: name, what it maps to, and whether it needs attention.
pub fn render_list(imports: &[RemoteImport]) -> String {
    if imports.is_empty() {
        return "No remotes found in this rclone config.".to_string();
    }
    let width = imports
        .iter()
        .map(|i| i.remote_name().chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for import in imports {
        out.push_str(&format!(
            "{:width$}  {}\n",
            import.remote_name(),
            import.summary(),
            width = width
        ));
    }
    let importable = imports
        .iter()
        .filter(|i| i.backend_kind().is_some())
        .count();
    out.push_str(&format!(
        "\n{importable} of {} remote(s) map to a Driven destination. \
         Run `driven-cli rclone import <name>` for the settings to enter.\n",
        imports.len()
    ));
    out
}

/// Everything Driven would use for one remote, as text.
pub fn render_detail(import: &RemoteImport, opts: &RenderOptions) -> String {
    let mut out = String::new();
    out.push_str(&format!("Remote: {}\n", import.remote_name()));
    match import {
        RemoteImport::S3(r) => render_s3_detail(r, opts, &mut out),
        RemoteImport::Drive(r) => render_drive_detail(r, opts, &mut out),
        RemoteImport::Unsupported {
            remote_type,
            reason,
            ..
        } => {
            out.push_str(&format!(
                "Type:   {}\n\nThis remote cannot be imported.\n  {reason}\n",
                if remote_type.is_empty() {
                    "(none)"
                } else {
                    remote_type
                }
            ));
        }
    }
    out
}

fn render_s3_detail(r: &S3Remote, opts: &RenderOptions, out: &mut String) {
    out.push_str("Type:   s3 -> Driven destination \"S3-compatible storage\"\n\n");
    out.push_str("Settings to enter in Driven:\n");
    push_field(
        out,
        "Endpoint",
        r.endpoint.as_deref().unwrap_or("(missing)"),
    );
    match &opts.bucket {
        Some(b) => push_field(out, "Bucket", b),
        None => push_field(
            out,
            "Bucket",
            "(not in rclone.conf - rclone puts the bucket in the PATH you type, \
             e.g. `remote:mybucket/dir`. Re-run with --bucket <name>.)",
        ),
    }
    push_field(out, "Region", &r.region);
    push_field(
        out,
        "Addressing",
        if r.path_style {
            "path style"
        } else {
            "virtual-host style"
        },
    );
    push_field(out, "Prefix", opts.prefix.as_deref().unwrap_or("(none)"));
    push_field(
        out,
        "Access key id",
        r.access_key_id.as_deref().unwrap_or("(missing)"),
    );
    push_field(
        out,
        "Secret access key",
        &match &r.secret_access_key {
            Some(s) => opts.show(s),
            None => "(missing)".to_string(),
        },
    );
    if let Some(p) = &r.provider {
        out.push_str(&format!("\n(rclone provider: {p})\n"));
    }
    // Redacting the secret is only helpful if the user is then told where it
    // is. Without this the default rendering would be a dead end and everyone
    // would reach for --reveal-secrets, which is the opposite of the intent.
    if !opts.reveal_secrets && r.secret_access_key.is_some() {
        out.push_str(&format!(
            "\nThe secret access key is the `secret_access_key` line of the [{}] section in {} - \
             copy it from there, or re-run with --reveal-secrets to print it here.\n",
            r.remote_name,
            opts.config_path.as_deref().unwrap_or("your rclone config"),
        ));
    }

    if let (Some(bucket), true) = (opts.bucket.as_deref(), r.is_complete()) {
        match r.to_config(bucket, opts.prefix.as_deref()) {
            Ok(cfg) => match cfg.to_json() {
                Ok(json) => {
                    out.push_str("\nDriven backend config (non-secret; the key pair goes to the OS keychain):\n  ");
                    out.push_str(&json);
                    out.push('\n');
                }
                Err(e) => out.push_str(&format!("\nCould not render the backend config: {e}\n")),
            },
            Err(e) => out.push_str(&format!("\nThese settings are not yet valid: {e}\n")),
        }
    }

    push_notes(out, &r.notes, &r.blockers);
    if r.is_complete() {
        out.push_str(
            "\nNext: Driven -> Add account -> S3-compatible storage, and enter the settings above. \
             The secret access key is stored in your OS keychain, never in Driven's config.\n",
        );
    }
}

fn render_drive_detail(r: &DriveRemote, opts: &RenderOptions, out: &mut String) {
    out.push_str("Type:   drive -> Driven destination \"Google Drive\"\n\n");
    out.push_str("Settings that carry across:\n");
    push_field(
        out,
        "Destination folder id",
        r.root_folder_id
            .as_deref()
            .unwrap_or("(none - pick one in Driven)"),
    );
    push_field(
        out,
        "Shared Drive id",
        r.team_drive.as_deref().unwrap_or("(none - My Drive)"),
    );
    if r.has_byo_client {
        out.push_str(
            "\nYour own OAuth client (reusable - it identifies the app, not the grant):\n",
        );
        push_field(out, "Client id", r.client_id().unwrap_or("(none)"));
        push_field(
            out,
            "Client secret",
            &match r.client_secret() {
                Some(s) => opts.show(s),
                None => "(none)".to_string(),
            },
        );
    }
    out.push_str("\nSettings that CANNOT carry across:\n");
    push_field(
        out,
        "OAuth token",
        if r.has_token {
            "present in rclone.conf, NOT imported (see below)"
        } else {
            "(none in rclone.conf)"
        },
    );
    push_notes(out, &r.notes, &r.blockers);
    out.push_str(
        "\nNext: add a Google Drive account in Driven (or `driven-cli auth --account <name>`) and \
         complete the sign-in. Then point the backup source at the folder id above.\n",
    );
}

fn push_field(out: &mut String, label: &str, value: &str) {
    out.push_str(&format!("  {label:<22} {value}\n"));
}

fn push_notes(out: &mut String, notes: &[String], blockers: &[String]) {
    if !blockers.is_empty() {
        out.push_str("\nNeeds your attention:\n");
        for b in blockers {
            out.push_str(&format!("  ! {b}\n"));
        }
    }
    if !notes.is_empty() {
        out.push_str("\nNotes:\n");
        for n in notes {
            out.push_str(&format!("  - {n}\n"));
        }
    }
}

/// The machine-readable form of [`render_detail`].
///
/// Stable enough to script against: `backend` is the Driven
/// [`BackendKind`](driven_remote::backend::BackendKind) id string, and for a
/// complete s3 remote with a bucket, `backendConfig` is byte-identical to what
/// Driven persists in `accounts.backend_config_json`.
pub fn to_json(import: &RemoteImport, opts: &RenderOptions) -> Value {
    let mut root = Map::new();
    root.insert("remote".into(), json!(import.remote_name()));
    root.insert(
        "backend".into(),
        match import.backend_kind() {
            Some(k) => json!(k.id()),
            None => Value::Null,
        },
    );
    match import {
        RemoteImport::S3(r) => {
            root.insert("importable".into(), json!(r.is_complete()));
            root.insert("provider".into(), json!(r.provider));
            root.insert("endpoint".into(), json!(r.endpoint));
            root.insert("region".into(), json!(r.region));
            root.insert("pathStyle".into(), json!(r.path_style));
            root.insert("bucket".into(), json!(opts.bucket));
            root.insert("prefix".into(), json!(opts.prefix));
            root.insert("accessKeyId".into(), json!(r.access_key_id));
            root.insert(
                "secretAccessKey".into(),
                match (&r.secret_access_key, opts.reveal_secrets) {
                    (Some(s), true) => json!(s.expose()),
                    // Present-but-hidden and absent must be DISTINGUISHABLE, or
                    // a script cannot tell "you need to reveal it" from "this
                    // remote has no credentials".
                    (Some(_), false) => json!(crate::secret::REDACTED),
                    (None, _) => Value::Null,
                },
            );
            if let (Some(bucket), true) = (opts.bucket.as_deref(), r.is_complete()) {
                if let Ok(cfg) = r.to_config(bucket, opts.prefix.as_deref()) {
                    if let Ok(text) = cfg.to_json() {
                        if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                            root.insert("backendConfig".into(), parsed);
                        }
                    }
                }
            }
            root.insert("notes".into(), json!(r.notes));
            root.insert("blockers".into(), json!(r.blockers));
        }
        RemoteImport::Drive(r) => {
            // Never `true`: a Drive remote always needs Driven's own OAuth flow.
            root.insert("importable".into(), json!(false));
            root.insert("rootFolderId".into(), json!(r.root_folder_id));
            root.insert("sharedDriveId".into(), json!(r.team_drive));
            root.insert("scope".into(), json!(r.scope));
            root.insert("hasOauthToken".into(), json!(r.has_token));
            root.insert("hasServiceAccount".into(), json!(r.has_service_account));
            root.insert("hasOwnOauthClient".into(), json!(r.has_byo_client));
            root.insert("clientId".into(), json!(r.client_id()));
            root.insert(
                "clientSecret".into(),
                match (r.client_secret(), opts.reveal_secrets) {
                    (Some(s), true) => json!(s.expose()),
                    (Some(_), false) => json!(crate::secret::REDACTED),
                    (None, _) => Value::Null,
                },
            );
            root.insert("notes".into(), json!(r.notes));
            root.insert("blockers".into(), json!(r.blockers));
        }
        RemoteImport::Unsupported {
            remote_type,
            reason,
            ..
        } => {
            root.insert("importable".into(), json!(false));
            root.insert("rcloneType".into(), json!(remote_type));
            root.insert("reason".into(), json!(reason.to_string()));
        }
    }
    Value::Object(root)
}

/// [`to_json`] rendered as pretty-printed text.
///
/// Exists so `driven-cli` needs no `serde` dependency of its own - the CLI is
/// deliberately a thin `println!` over this crate (the same discipline that
/// keeps `reqwest`/`bytes`/`serde` out of it for the Drive paths).
pub fn to_json_pretty(import: &RemoteImport, opts: &RenderOptions) -> String {
    // `Value` -> string cannot fail; the fallback keeps this infallible for the
    // caller rather than inventing an error path nothing can trigger.
    serde_json::to_string_pretty(&to_json(import, opts))
        .unwrap_or_else(|_| to_json(import, opts).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_file::RcloneConfig;
    use crate::import::classify;

    const R2: &str = "[r2]\ntype = s3\nprovider = Cloudflare\naccess_key_id = AKIDEXAMPLE\nsecret_access_key = TOPSECRETVALUE\nendpoint = https://acct.r2.cloudflarestorage.com\nregion = auto\n";
    const GDRIVE: &str = "[gd]\ntype = drive\nroot_folder_id = 1AbC\nteam_drive = 0XYZ\nclient_id = cid.apps.googleusercontent.com\nclient_secret = GOCSPX-TOPSECRETCLIENT\ntoken = {\"refresh_token\":\"1//0eTOPSECRETTOKEN\"}\n";

    fn one(text: &str) -> RemoteImport {
        let cfg = RcloneConfig::parse(text).unwrap();
        classify(&cfg.remotes()[0])
    }

    #[test]
    fn the_listing_shows_every_remote_and_a_count() {
        let cfg = RcloneConfig::parse(
            "[r2]\ntype = s3\nprovider = Cloudflare\naccess_key_id = k\nsecret_access_key = s\nendpoint = https://e.example\n[gd]\ntype = drive\n[nas]\ntype = sftp\n",
        )
        .unwrap();
        let listing = render_list(&crate::classify_all(&cfg));
        for name in ["r2", "gd", "nas"] {
            assert!(listing.contains(name), "{name} missing from:\n{listing}");
        }
        assert!(listing.contains("2 of 3 remote(s)"), "{listing}");
        assert!(listing.contains("not importable"), "{listing}");
    }

    #[test]
    fn an_empty_listing_says_so_rather_than_printing_nothing() {
        assert!(render_list(&[]).contains("No remotes found"));
    }

    #[test]
    fn the_s3_detail_hides_the_secret_by_default_and_shows_it_on_request() {
        let import = one(R2);
        let hidden = render_detail(&import, &RenderOptions::default());
        assert!(
            !hidden.contains("TOPSECRETVALUE"),
            "the default rendering leaked a secret:\n{hidden}"
        );
        assert!(hidden.contains(crate::secret::REDACTED));
        assert!(
            hidden.contains("AKIDEXAMPLE"),
            "the key ID is not the secret"
        );
        assert!(hidden.contains("https://acct.r2.cloudflarestorage.com"));
        assert!(hidden.contains("path style"));

        let shown = render_detail(
            &import,
            &RenderOptions {
                reveal_secrets: true,
                ..Default::default()
            },
        );
        assert!(shown.contains("TOPSECRETVALUE"));
    }

    #[test]
    fn the_redacted_rendering_says_where_the_secret_is_so_it_is_not_a_dead_end() {
        // Hiding the value is only helpful if the user is told how to get it;
        // otherwise everyone reaches for --reveal-secrets and the default is
        // decorative.
        // `config_path` is an already-rendered String that the renderer echoes
        // verbatim - no `PathBuf::join`, so no platform separator is introduced
        // and this literal round-trips identically on Windows. (Contrast
        // `config_file::tests::locate_config_...`, which compares against paths
        // the code BUILDS with `join` and so must build its expectations the
        // same way.) Bound to a variable so the assertion cannot drift from it.
        let config_path = "/home/u/.config/rclone/rclone.conf";
        let out = render_detail(
            &one(R2),
            &RenderOptions {
                config_path: Some(config_path.into()),
                ..Default::default()
            },
        );
        assert!(
            out.contains("secret_access_key` line of the [r2] section"),
            "{out}"
        );
        assert!(out.contains(config_path), "{out}");
        assert!(out.contains("--reveal-secrets"), "{out}");
        assert!(!out.contains("TOPSECRETVALUE"), "{out}");

        // With --reveal-secrets the pointer is redundant and must not appear.
        let shown = render_detail(
            &one(R2),
            &RenderOptions {
                reveal_secrets: true,
                config_path: Some("/x.conf".into()),
                ..Default::default()
            },
        );
        assert!(!shown.contains("copy it from there"), "{shown}");

        // Without a known path the wording still makes sense.
        let out = render_detail(&one(R2), &RenderOptions::default());
        assert!(out.contains("your rclone config"), "{out}");

        // A remote with NO secret gets no pointer to a line that is not there.
        let out = render_detail(
            &one("[a]\ntype = s3\nprovider = Ceph\nendpoint = https://e.example\n"),
            &RenderOptions::default(),
        );
        assert!(!out.contains("copy it from there"), "{out}");
    }

    #[test]
    fn an_s3_remote_needing_attention_names_the_command_that_shows_why() {
        // `render_list` prints one line per remote and then the count, so a
        // summary saying "see below" would point at nothing.
        let listing = render_list(&[one("[ceph]\ntype = s3\nprovider = Ceph\nenv_auth = true\n")]);
        assert!(listing.contains("imports partially"), "{listing}");
        assert!(listing.contains("rclone import ceph"), "{listing}");
        assert!(!listing.contains("see below"), "{listing}");
    }

    #[test]
    fn without_a_bucket_the_detail_explains_where_rclone_keeps_it() {
        let out = render_detail(&one(R2), &RenderOptions::default());
        assert!(out.contains("--bucket"), "{out}");
        assert!(out.contains("remote:mybucket/dir"), "{out}");
        assert!(
            !out.contains("backendConfig") && !out.contains("\"endpoint\""),
            "no config blob without a bucket:\n{out}"
        );
    }

    #[test]
    fn with_a_bucket_the_detail_emits_the_exact_persisted_config_blob() {
        let opts = RenderOptions {
            bucket: Some("backups".into()),
            prefix: Some("laptop".into()),
            ..Default::default()
        };
        let out = render_detail(&one(R2), &opts);
        assert!(out.contains("\"bucket\":\"backups\""), "{out}");
        assert!(out.contains("\"prefix\":\"laptop/\""), "{out}");
        assert!(out.contains("\"pathStyle\":true"), "{out}");
        assert!(
            !out.contains("TOPSECRETVALUE"),
            "the persisted blob must never carry the secret:\n{out}"
        );
        assert!(out.contains("OS keychain"));
    }

    #[test]
    fn an_incomplete_s3_remote_renders_its_blockers_and_no_config_blob() {
        let out = render_detail(
            &one("[a]\ntype = s3\nprovider = Ceph\nenv_auth = true\n"),
            &RenderOptions {
                bucket: Some("b".into()),
                ..Default::default()
            },
        );
        assert!(out.contains("Needs your attention"), "{out}");
        assert!(out.contains("env_auth"), "{out}");
        assert!(out.contains("(missing)"), "{out}");
        assert!(!out.contains("backendConfig"), "{out}");
    }

    #[test]
    fn the_drive_detail_never_prints_the_token_even_with_reveal_secrets() {
        let import = one(GDRIVE);
        for reveal in [false, true] {
            let out = render_detail(
                &import,
                &RenderOptions {
                    reveal_secrets: reveal,
                    ..Default::default()
                },
            );
            assert!(
                !out.contains("1//0eTOPSECRETTOKEN"),
                "the OAuth token must never be rendered (reveal={reveal}):\n{out}"
            );
            assert!(out.contains("1AbC"), "the folder id carries across");
            assert!(out.contains("0XYZ"), "the shared drive id carries across");
            assert!(out.contains("NOT imported"));
        }
        // The BYO client secret DOES follow the reveal flag - it is reusable.
        let hidden = render_detail(&import, &RenderOptions::default());
        assert!(!hidden.contains("GOCSPX-TOPSECRETCLIENT"));
        let shown = render_detail(
            &import,
            &RenderOptions {
                reveal_secrets: true,
                ..Default::default()
            },
        );
        assert!(shown.contains("GOCSPX-TOPSECRETCLIENT"));
    }

    #[test]
    fn a_drive_remote_without_a_byo_client_omits_that_block() {
        let out = render_detail(
            &one("[gd]\ntype = drive\ntoken = {\"refresh_token\":\"1//0e\"}\n"),
            &RenderOptions::default(),
        );
        assert!(!out.contains("Client id"), "{out}");
        assert!(out.contains("My Drive"), "{out}");
        assert!(out.contains("pick one in Driven"), "{out}");
    }

    #[test]
    fn an_unsupported_remote_renders_the_reason() {
        let out = render_detail(
            &one("[c]\ntype = crypt\nremote = other:\n"),
            &RenderOptions::default(),
        );
        assert!(out.contains("cannot be imported"), "{out}");
        assert!(out.contains("wrapper"), "{out}");
        let out = render_detail(&one("[w]\nfoo = bar\n"), &RenderOptions::default());
        assert!(
            out.contains("(none)"),
            "a type-less section renders too:\n{out}"
        );
    }

    #[test]
    fn the_json_form_redacts_by_default_and_distinguishes_hidden_from_absent() {
        let v = to_json(&one(R2), &RenderOptions::default());
        assert_eq!(v["backend"], json!("s3"));
        assert_eq!(v["importable"], json!(true));
        assert_eq!(v["region"], json!("auto"));
        assert_eq!(v["pathStyle"], json!(true));
        assert_eq!(v["accessKeyId"], json!("AKIDEXAMPLE"));
        assert_eq!(v["secretAccessKey"], json!(crate::secret::REDACTED));
        assert!(
            !v.to_string().contains("TOPSECRETVALUE"),
            "the JSON leaked a secret: {v}"
        );

        // Absent credentials are null, NOT the redaction placeholder.
        let v = to_json(
            &one("[a]\ntype = s3\nprovider = Ceph\nendpoint = https://e.example\n"),
            &RenderOptions::default(),
        );
        assert_eq!(v["secretAccessKey"], Value::Null);
        assert_eq!(v["accessKeyId"], Value::Null);
        assert_eq!(v["importable"], json!(false));
        assert!(!v["blockers"].as_array().unwrap().is_empty());
    }

    #[test]
    fn the_json_backend_config_matches_what_driven_persists() {
        let v = to_json(
            &one(R2),
            &RenderOptions {
                bucket: Some("backups".into()),
                prefix: Some("/laptop/".into()),
                reveal_secrets: true,
                config_path: None,
            },
        );
        assert_eq!(v["secretAccessKey"], json!("TOPSECRETVALUE"));
        let cfg = &v["backendConfig"];
        assert_eq!(
            cfg["endpoint"],
            json!("https://acct.r2.cloudflarestorage.com")
        );
        assert_eq!(cfg["bucket"], json!("backups"));
        assert_eq!(cfg["region"], json!("auto"));
        assert_eq!(cfg["pathStyle"], json!(true));
        assert_eq!(cfg["prefix"], json!("laptop/"), "normalized by S3Config");
        assert!(
            cfg.get("secretAccessKey").is_none() && cfg.get("accessKeyId").is_none(),
            "the persisted blob must contain no credential: {cfg}"
        );
    }

    #[test]
    fn the_json_drive_form_reports_the_grant_without_carrying_it() {
        let v = to_json(&one(GDRIVE), &RenderOptions::default());
        assert_eq!(v["backend"], json!("google_drive"));
        assert_eq!(
            v["importable"],
            json!(false),
            "a Drive remote is NEVER fully importable"
        );
        assert_eq!(v["hasOauthToken"], json!(true));
        assert_eq!(v["rootFolderId"], json!("1AbC"));
        assert_eq!(v["sharedDriveId"], json!("0XYZ"));
        assert_eq!(v["hasOwnOauthClient"], json!(true));
        assert!(v.get("token").is_none(), "there is no token field at all");
        assert!(!v.to_string().contains("1//0eTOPSECRETTOKEN"));
        assert!(!v.to_string().contains("GOCSPX-TOPSECRETCLIENT"));

        // Even with reveal_secrets, the token is not a field that exists.
        let v = to_json(
            &one(GDRIVE),
            &RenderOptions {
                reveal_secrets: true,
                ..Default::default()
            },
        );
        assert!(!v.to_string().contains("1//0eTOPSECRETTOKEN"));
        assert_eq!(v["clientSecret"], json!("GOCSPX-TOPSECRETCLIENT"));
    }

    #[test]
    fn the_json_unsupported_form_carries_the_reason() {
        let v = to_json(
            &one("[n]\ntype = b2\naccount = 1\n"),
            &RenderOptions::default(),
        );
        assert_eq!(v["backend"], Value::Null);
        assert_eq!(v["importable"], json!(false));
        assert_eq!(v["rcloneType"], json!("b2"));
        assert!(v["reason"].as_str().unwrap().contains("backblazeb2.com"));
    }
}
