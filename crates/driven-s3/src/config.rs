//! S3 destination configuration and its keychain-backed credentials.
//!
//! ## What lives where
//!
//! - [`S3Config`] - the NON-SECRET settings (endpoint, bucket, region,
//!   addressing style, key prefix). Persisted as JSON in
//!   `accounts.backend_config_json`.
//! - [`S3Credentials`] - the access key id + secret access key. Persisted ONLY
//!   in the OS keychain, by [`S3CredentialStore`], keyed by the account id -
//!   the same shape `driven_drive::google::token_store::ClientCredsStore` uses
//!   for the BYO OAuth client secret.
//!
//! The secret NEVER enters SQLite, a config file, a log line, or a `Debug`
//! rendering: [`S3Credentials`] has a hand-written `Debug` that redacts both
//! fields (the access key id is redacted too - it is not a secret, but a log
//! line pairing it with a bucket and endpoint is a nudge toward the one that
//! is).

use serde::{Deserialize, Serialize};

/// Keychain service namespace for the S3 access key id + secret access key,
/// mirroring the `driven.google.*` namespaces.
const KEYRING_S3_CREDENTIALS_SERVICE: &str = "driven.s3.credentials";

/// The region to sign with when the user does not name one.
///
/// `us-east-1` rather than the tempting `auto`: it is the SigV4 region every
/// S3-compatible service accepts as the neutral default (R2 documents
/// `auto` and `us-east-1` as interchangeable; MinIO ignores the region unless
/// configured; AWS itself requires a real region, and `us-east-1` is the one
/// legacy global endpoint). Signing `auto` against AWS proper would fail, so
/// the safe default is the real region name.
pub const DEFAULT_REGION: &str = "us-east-1";

/// Non-secret configuration for an S3-compatible destination.
///
/// Serialized into `accounts.backend_config_json`. Field names are part of the
/// stored format (v1.0.0 stability) - add fields with `#[serde(default)]`, never
/// rename one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct S3Config {
    /// Service endpoint, including scheme. Examples:
    /// `https://s3.us-west-2.amazonaws.com`,
    /// `https://<account>.r2.cloudflarestorage.com`,
    /// `http://127.0.0.1:9000` (MinIO).
    pub endpoint: String,
    /// Bucket name.
    pub bucket: String,
    /// SigV4 signing region. Defaults to [`DEFAULT_REGION`] when omitted.
    #[serde(default = "default_region")]
    pub region: String,
    /// `true` to address the bucket as a PATH segment
    /// (`https://host/bucket/key`), `false` for virtual-host style
    /// (`https://bucket.host/key`).
    ///
    /// Defaults to `true`, which is the choice that works everywhere:
    /// - MinIO's default deployment only serves path style.
    /// - Cloudflare R2's account endpoint only serves path style (its
    ///   virtual-host form is a different, per-bucket hostname).
    /// - AWS S3 still serves path style for existing buckets and always serves
    ///   it for regional endpoints.
    ///
    /// Virtual-host style is offered for AWS-proper deployments that have
    /// disabled path style, and for buckets whose names are not DNS-safe in the
    /// other direction.
    #[serde(default = "default_path_style")]
    pub path_style: bool,
    /// Optional key prefix to confine Driven to a subtree of the bucket. Stored
    /// without a leading slash; a trailing slash is added when absent. `None`
    /// or empty means the bucket root.
    #[serde(default)]
    pub prefix: Option<String>,
}

fn default_region() -> String {
    DEFAULT_REGION.to_string()
}

const fn default_path_style() -> bool {
    true
}

/// A validation failure in an [`S3Config`] the user supplied.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum S3ConfigError {
    /// The endpoint was empty or not a parseable absolute URL.
    #[error("s3.config_invalid: endpoint must be an absolute http(s) URL")]
    Endpoint,
    /// The endpoint used a scheme other than http/https.
    #[error("s3.config_invalid: endpoint scheme must be http or https")]
    Scheme,
    /// The bucket name was empty or contained a path separator.
    #[error("s3.config_invalid: bucket must be a non-empty name with no '/'")]
    Bucket,
    /// The prefix escaped its own subtree (`..`) or contained a NUL.
    #[error("s3.config_invalid: prefix must not contain '..' or NUL")]
    Prefix,
}

impl S3Config {
    /// Validate and NORMALIZE user input into a config safe to persist.
    ///
    /// Normalization is deliberately part of validation so the stored value is
    /// canonical and every later use (URL building, key joining) can assume it:
    /// the endpoint loses any trailing slash, the region falls back to
    /// [`DEFAULT_REGION`] when blank, and the prefix loses leading slashes and
    /// gains exactly one trailing slash (or becomes `None`).
    pub fn normalized(mut self) -> Result<Self, S3ConfigError> {
        let endpoint = self.endpoint.trim().trim_end_matches('/').to_string();
        let parsed = url::Url::parse(&endpoint).map_err(|_| S3ConfigError::Endpoint)?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(S3ConfigError::Scheme);
        }
        if parsed.host_str().is_none() {
            return Err(S3ConfigError::Endpoint);
        }
        self.endpoint = endpoint;

        let bucket = self.bucket.trim().to_string();
        if bucket.is_empty() || bucket.contains('/') {
            return Err(S3ConfigError::Bucket);
        }
        self.bucket = bucket;

        let region = self.region.trim().to_string();
        self.region = if region.is_empty() {
            DEFAULT_REGION.to_string()
        } else {
            region
        };

        self.prefix = match self.prefix.take() {
            None => None,
            Some(raw) => {
                let trimmed = raw.trim().trim_start_matches('/');
                if trimmed.is_empty() {
                    None
                } else {
                    // `..` in a key prefix cannot traverse out of the bucket on
                    // the server (S3 keys are opaque strings, not paths), but it
                    // WOULD produce keys that a naive local mirror-restore would
                    // resolve outside the destination. Reject it at the boundary.
                    if trimmed.split('/').any(|seg| seg == "..") || trimmed.contains('\0') {
                        return Err(S3ConfigError::Prefix);
                    }
                    let mut p = trimmed.trim_end_matches('/').to_string();
                    p.push('/');
                    Some(p)
                }
            }
        };

        Ok(self)
    }

    /// The key prefix as a plain string: `""` for the bucket root, otherwise a
    /// slash-terminated prefix. This is also the id of the destination ROOT
    /// "folder" (see the module docs on `crate::store`).
    pub fn root_prefix(&self) -> &str {
        self.prefix.as_deref().unwrap_or("")
    }

    /// Parse an `accounts.backend_config_json` blob, validating + normalizing it.
    pub fn from_json(json: &str) -> anyhow::Result<Self> {
        let cfg: S3Config = serde_json::from_str(json).map_err(|e| {
            anyhow::anyhow!("s3.config_invalid: could not parse backend config: {e}")
        })?;
        Ok(cfg.normalized()?)
    }

    /// Render to the `accounts.backend_config_json` blob.
    pub fn to_json(&self) -> anyhow::Result<String> {
        serde_json::to_string(self).map_err(|e| {
            anyhow::anyhow!("s3.config_invalid: could not serialize backend config: {e}")
        })
    }
}

/// An S3 access key pair. Lives in the OS keychain only.
#[derive(Clone, PartialEq, Eq)]
pub struct S3Credentials {
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
}

// Hand-written so neither field can ever reach a log line, a panic message, or
// the diagnostic bundle through a derived `Debug`.
impl std::fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Credentials")
            .field("access_key_id", &"<redacted>")
            .field("secret_access_key", &"<redacted>")
            .finish()
    }
}

/// Keychain-backed store for one account's [`S3Credentials`].
///
/// One entry holding `access_key_id\nsecret_access_key`, exactly like
/// `ClientCredsStore`'s `client_id\nclient_secret` encoding - the same reason
/// applies (an OS keychain entry is a single string, and one entry keeps the
/// pair atomic so a half-rotated credential is impossible). Callers MUST reject
/// control characters in either field before storing, or the `\n` delimiter is
/// ambiguous on read-back.
pub struct S3CredentialStore {
    account: String,
}

/// Run a keyring operation on a scratch thread on Linux.
///
/// The Linux `keyring` backend (Secret Service over DBus) internally
/// `block_on`s its own async runtime; called from a tokio worker thread
/// (IPC commands, assembly, the CLI's async main) that panics with
/// "Cannot start a runtime from within a runtime" and kills the caller
/// mid-flight. Surfaced by the agent-QA e2e harness (create_s3_account hung
/// its invoke inside the Linux container; driven-cli reproduced directly).
/// Keychain ops are rare and short-lived, so a scoped scratch thread - no
/// ambient runtime - is cheap and keeps the public signatures sync. macOS /
/// Windows backends are plain native calls and stay on the calling thread.
fn keyring_off_runtime<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    #[cfg(target_os = "linux")]
    {
        std::thread::scope(|s| {
            s.spawn(f)
                .join()
                .expect("keyring operation thread panicked")
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        f()
    }
}

impl S3CredentialStore {
    /// A store for `account` (the `accounts.id` UUID string).
    pub fn new(account: impl Into<String>) -> Self {
        Self {
            account: account.into(),
        }
    }

    fn entry(&self) -> anyhow::Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_S3_CREDENTIALS_SERVICE, &self.account)
            .map_err(|e| anyhow::anyhow!("failed to open the S3 credential keychain entry: {e}"))
    }

    

    /// Persist the credential pair, replacing any existing one.
    pub fn store(&self, creds: &S3Credentials) -> anyhow::Result<()> {
        let encoded = encode_credentials(creds)?;
        keyring_off_runtime(|| {
            self.entry()?.set_password(&encoded).map_err(|e| {
                anyhow::anyhow!("failed to store the S3 credentials in the keychain: {e}")
            })
        })
    }

    /// Load the credential pair, or `None` when the account has none stored.
    pub fn load(&self) -> anyhow::Result<Option<S3Credentials>> {
        keyring_off_runtime(|| match self.entry()?.get_password() {
            Ok(raw) => Ok(Some(decode_credentials(&raw)?)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!(
                "failed to read the S3 credentials from the keychain: {e}"
            )),
        })
    }

    /// Remove the credential pair. A missing entry is a no-op (idempotent, so
    /// account removal can call it unconditionally).
    pub fn delete(&self) -> anyhow::Result<()> {
        keyring_off_runtime(|| match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow::anyhow!(
                "failed to delete the S3 credentials from the keychain: {e}"
            )),
        })
    }
}

/// Encode a credential pair for the single keychain entry. Rejects control
/// characters, which would make the `\n` delimiter ambiguous on read-back.
fn encode_credentials(creds: &S3Credentials) -> anyhow::Result<String> {
    for (label, value) in [
        ("access key id", &creds.access_key_id),
        ("secret access key", &creds.secret_access_key),
    ] {
        if value.chars().any(char::is_control) {
            anyhow::bail!("s3.config_invalid: the {label} must not contain control characters");
        }
    }
    Ok(format!(
        "{}\n{}",
        creds.access_key_id, creds.secret_access_key
    ))
}

/// Decode the single keychain entry back into a pair. Splits on the FIRST
/// newline only.
fn decode_credentials(raw: &str) -> anyhow::Result<S3Credentials> {
    let (id, secret) = raw.split_once('\n').ok_or_else(|| {
        anyhow::anyhow!("s3.config_invalid: malformed S3 credential keychain record")
    })?;
    Ok(S3Credentials {
        access_key_id: id.to_string(),
        secret_access_key: secret.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(endpoint: &str, bucket: &str, prefix: Option<&str>) -> S3Config {
        S3Config {
            endpoint: endpoint.to_string(),
            bucket: bucket.to_string(),
            region: String::new(),
            path_style: true,
            prefix: prefix.map(str::to_string),
        }
    }

    #[test]
    fn normalization_canonicalizes_endpoint_region_and_prefix() {
        let c = cfg(
            "  https://example.com:9000/  ",
            " my-bucket ",
            Some("/a/b//"),
        )
        .normalized()
        .expect("valid");
        assert_eq!(c.endpoint, "https://example.com:9000");
        assert_eq!(c.bucket, "my-bucket");
        assert_eq!(c.region, DEFAULT_REGION, "a blank region falls back");
        assert_eq!(c.prefix.as_deref(), Some("/a/b/".trim_start_matches('/')));
        assert_eq!(c.root_prefix(), "a/b/");
    }

    #[test]
    fn an_empty_prefix_is_the_bucket_root() {
        for raw in [None, Some(""), Some("   "), Some("/")] {
            let c = cfg("https://example.com", "b", raw).normalized().unwrap();
            assert_eq!(c.prefix, None, "prefix {raw:?} must normalize to None");
            assert_eq!(c.root_prefix(), "");
        }
    }

    #[test]
    fn invalid_configs_are_rejected() {
        assert_eq!(
            cfg("", "b", None).normalized().unwrap_err(),
            S3ConfigError::Endpoint
        );
        assert_eq!(
            cfg("not a url", "b", None).normalized().unwrap_err(),
            S3ConfigError::Endpoint
        );
        assert_eq!(
            cfg("ftp://example.com", "b", None)
                .normalized()
                .unwrap_err(),
            S3ConfigError::Scheme
        );
        assert_eq!(
            cfg("https://example.com", "", None)
                .normalized()
                .unwrap_err(),
            S3ConfigError::Bucket
        );
        assert_eq!(
            cfg("https://example.com", "a/b", None)
                .normalized()
                .unwrap_err(),
            S3ConfigError::Bucket
        );
        assert_eq!(
            cfg("https://example.com", "b", Some("a/../../etc"))
                .normalized()
                .unwrap_err(),
            S3ConfigError::Prefix
        );
    }

    #[test]
    fn json_round_trips_and_applies_defaults() {
        let c = cfg("https://example.com", "b", Some("p"))
            .normalized()
            .unwrap();
        let json = c.to_json().unwrap();
        assert_eq!(S3Config::from_json(&json).unwrap(), c);

        // A blob written without the optional fields still loads (the stored
        // format must tolerate the minimum the UI can produce).
        let minimal = S3Config::from_json(r#"{"endpoint":"https://e.example","bucket":"b"}"#)
            .expect("minimal config");
        assert_eq!(minimal.region, DEFAULT_REGION);
        assert!(minimal.path_style, "path style is the portable default");
        assert_eq!(minimal.prefix, None);
    }

    #[test]
    fn the_config_blob_never_carries_credentials() {
        // Guard against someone adding a secret field to the persisted config:
        // it would land in SQLite, the diagnostic bundle and any backup of it.
        let json = cfg("https://example.com", "b", None)
            .normalized()
            .unwrap()
            .to_json()
            .unwrap();
        for forbidden in ["secret", "accessKey", "access_key", "password", "token"] {
            assert!(
                !json.to_lowercase().contains(&forbidden.to_lowercase()),
                "the persisted S3 config must not contain {forbidden:?}: {json}"
            );
        }
    }

    #[test]
    fn credentials_redact_both_fields_in_debug() {
        let creds = S3Credentials {
            access_key_id: "AKIAEXAMPLE".to_string(),
            secret_access_key: "super-secret-value".to_string(),
        };
        let rendered = format!("{creds:?}");
        assert!(!rendered.contains("AKIAEXAMPLE"));
        assert!(!rendered.contains("super-secret-value"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn credential_encoding_round_trips_and_rejects_control_chars() {
        let creds = S3Credentials {
            access_key_id: "id".to_string(),
            secret_access_key: "sec/ret+with=padding".to_string(),
        };
        let encoded = encode_credentials(&creds).unwrap();
        assert_eq!(decode_credentials(&encoded).unwrap(), creds);

        for bad in ["a\nb", "a\rb", "a\tb", "a\0b"] {
            assert!(
                encode_credentials(&S3Credentials {
                    access_key_id: bad.to_string(),
                    secret_access_key: "s".to_string(),
                })
                .is_err(),
                "{bad:?} must be rejected"
            );
            assert!(encode_credentials(&S3Credentials {
                access_key_id: "i".to_string(),
                secret_access_key: bad.to_string(),
            })
            .is_err());
        }
    }

    #[test]
    fn decoding_a_malformed_record_is_an_error_not_a_silent_empty_secret() {
        assert!(decode_credentials("no-newline-here").is_err());
    }

    /// Isolate this test from the OS keychain, so the credential round trip
    /// runs for real against an in-memory store without touching (or requiring)
    /// a real one. Returns `None` when isolation could not be established, in
    /// which case the caller MUST skip - see [`driven_test_fixtures::keychain`]
    /// for why writing for real is not an acceptable fallback (on macOS it
    /// blocks the run on a modal permission prompt).
    ///
    /// The returned guard also serializes these tests: the default credential
    /// store is process-global, so they key their entries by a unique account
    /// id and take turns.
    fn keychain() -> Option<driven_test_fixtures::keychain::KeychainGuard> {
        driven_test_fixtures::keychain::isolated()
    }

    #[test]
    fn the_test_suite_is_isolated_from_the_os_keychain() {
        // The guard for this whole crate: if the isolation mechanism ever stops
        // working (a `keyring` upgrade changing how the default store is
        // installed, say), fail HERE and loudly rather than letting the round
        // trips below silently skip - or, worse, start writing into a real
        // login keychain.
        assert!(
            driven_test_fixtures::keychain::is_isolated(),
            "the in-memory keyring store must be the effective default store"
        );
    }

    #[test]
    fn credentials_round_trip_through_the_keychain() {
        let Some(_g) = keychain() else { return };
        let store = S3CredentialStore::new("acct-s3-round-trip");
        assert!(
            store
                .load()
                .expect("an empty store is not an error")
                .is_none(),
            "an account with no stored key pair reads back as None, not an error"
        );

        let creds = S3Credentials {
            access_key_id: "AKIAEXAMPLE".to_string(),
            secret_access_key: "sec/ret+with=padding".to_string(),
        };
        store.store(&creds).expect("store");
        assert_eq!(store.load().expect("load"), Some(creds.clone()));

        // Rotation replaces rather than appends.
        let rotated = S3Credentials {
            access_key_id: "AKIAROTATED".to_string(),
            secret_access_key: "new-secret".to_string(),
        };
        store.store(&rotated).expect("rotate");
        assert_eq!(store.load().expect("load after rotate"), Some(rotated));
    }

    #[test]
    fn deleting_credentials_is_idempotent() {
        // Account removal calls this unconditionally, including for accounts
        // that never had an S3 key pair, so a missing entry must not error.
        let Some(_g) = keychain() else { return };
        let store = S3CredentialStore::new("acct-s3-delete");
        store.delete().expect("deleting a missing entry is a no-op");

        store
            .store(&S3Credentials {
                access_key_id: "id".to_string(),
                secret_access_key: "secret".to_string(),
            })
            .expect("store");
        store.delete().expect("delete");
        assert!(store.load().expect("load after delete").is_none());
        store.delete().expect("a second delete is still a no-op");
    }

    #[test]
    fn credentials_are_scoped_per_account() {
        // The keychain key is the account id, so two S3 accounts in one install
        // must not read each other's key pair.
        let Some(_g) = keychain() else { return };
        let a = S3CredentialStore::new("acct-a");
        let b = S3CredentialStore::new("acct-b");
        a.store(&S3Credentials {
            access_key_id: "a-id".to_string(),
            secret_access_key: "a-secret".to_string(),
        })
        .expect("store a");
        b.store(&S3Credentials {
            access_key_id: "b-id".to_string(),
            secret_access_key: "b-secret".to_string(),
        })
        .expect("store b");

        assert_eq!(a.load().unwrap().unwrap().access_key_id, "a-id");
        assert_eq!(b.load().unwrap().unwrap().access_key_id, "b-id");
        a.delete().expect("delete a");
        assert!(a.load().unwrap().is_none());
        assert!(b.load().unwrap().is_some(), "deleting a must not touch b");
    }

    #[test]
    fn storing_a_credential_with_control_characters_is_refused() {
        // The keychain record is `id\nsecret`; a newline inside either field
        // would decode to a DIFFERENT pair on read-back, silently swapping the
        // account's credentials after a restart.
        let Some(_g) = keychain() else { return };
        let store = S3CredentialStore::new("acct-control-chars");
        assert!(store
            .store(&S3Credentials {
                access_key_id: "id\nwith-newline".to_string(),
                secret_access_key: "secret".to_string(),
            })
            .is_err());
        assert!(
            store.load().expect("load").is_none(),
            "a refused store must leave nothing behind"
        );
    }
}
