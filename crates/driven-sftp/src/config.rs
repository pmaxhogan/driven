//! SFTP destination configuration and its keychain-backed credential.
//!
//! ## What lives where
//!
//! - [`SftpConfig`] - the NON-SECRET settings (host, port, root path,
//!   username, which auth method is configured, the pinned host-key
//!   fingerprint). Persisted as JSON in `accounts.backend_config_json`.
//! - [`SftpCredential`] - the password, or the private key + optional
//!   passphrase. Persisted ONLY in the OS keychain, by
//!   [`SftpCredentialStore`], keyed by the account id - the same shape
//!   `driven_s3::config::S3CredentialStore` uses for the S3 access key pair.
//!
//! The secret NEVER enters SQLite, a config file, a log line, or a `Debug`
//! rendering: [`SftpCredential`] has a hand-written `Debug` that redacts every
//! field that could hold key material.

use serde::{Deserialize, Serialize};

/// Keychain service namespace for the SFTP password / private key.
const KEYRING_SFTP_CREDENTIALS_SERVICE: &str = "driven.sftp.credentials";

/// The SSH port to connect on when the user does not name one.
pub const DEFAULT_PORT: u16 = 22;

/// Which auth method an [`SftpConfig`] is set up to use. This is a TAG only -
/// the actual secret material lives in the keychain as an [`SftpCredential`],
/// never here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SftpAuthKind {
    /// A plain SSH password.
    Password,
    /// An SSH private key (optionally passphrase-protected).
    PrivateKey,
}

/// Non-secret configuration for an SFTP destination.
///
/// Serialized into `accounts.backend_config_json`. Field names are part of
/// the stored format (v1.0.0 stability) - add fields with
/// `#[serde(default)]`, never rename one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpConfig {
    /// The SSH server hostname or IP address (no scheme, no port).
    pub host: String,
    /// The SSH port. Defaults to [`DEFAULT_PORT`] when omitted.
    #[serde(default = "default_port")]
    pub port: u16,
    /// The remote path Driven treats as the destination root. An empty value
    /// normalizes to `"/"`.
    pub root_path: String,
    /// The SSH username to authenticate as.
    pub username: String,
    /// Which auth method the account uses. The secret itself lives in the
    /// keychain via [`SftpCredentialStore`].
    pub auth: SftpAuthKind,
    /// The host-key fingerprint pinned on the creation probe (TOFU). `None`
    /// only transiently, before the first successful probe.
    #[serde(default)]
    pub host_key_fingerprint: Option<String>,
}

const fn default_port() -> u16 {
    DEFAULT_PORT
}

/// A validation failure in an [`SftpConfig`] the user supplied.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SftpConfigError {
    /// The host was empty.
    #[error("sftp.config_invalid: host must not be empty")]
    Host,
    /// The username was empty or contained control characters.
    #[error("sftp.config_invalid: username must not be empty or contain control characters")]
    Username,
    /// The root path escaped its own subtree (`..`) or contained a NUL.
    #[error("sftp.config_invalid: root_path must not contain '..' or NUL")]
    RootPath,
}

impl SftpConfig {
    /// Validate and NORMALIZE user input into a config safe to persist.
    ///
    /// Normalization is deliberately part of validation so the stored value
    /// is canonical and every later use (path joining, display) can assume
    /// it: the host and username lose surrounding whitespace, and the root
    /// path loses a trailing slash (unless it is the bare root `"/"`) and an
    /// empty root path becomes `"/"`.
    pub fn normalized(mut self) -> Result<Self, SftpConfigError> {
        let host = self.host.trim().to_string();
        if host.is_empty() {
            return Err(SftpConfigError::Host);
        }
        self.host = host;

        let username = self.username.trim().to_string();
        if username.is_empty() || username.chars().any(char::is_control) {
            return Err(SftpConfigError::Username);
        }
        self.username = username;

        let root = self.root_path.trim();
        if root.contains('\0') || root.split('/').any(|seg| seg == "..") {
            return Err(SftpConfigError::RootPath);
        }
        self.root_path = if root.is_empty() {
            "/".to_string()
        } else if root.len() > 1 {
            root.trim_end_matches('/').to_string()
        } else {
            root.to_string()
        };

        self.host_key_fingerprint = self
            .host_key_fingerprint
            .take()
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty());

        Ok(self)
    }

    /// Parse an `accounts.backend_config_json` blob, validating + normalizing
    /// it.
    pub fn from_json(json: &str) -> anyhow::Result<Self> {
        let cfg: SftpConfig = serde_json::from_str(json).map_err(|e| {
            anyhow::anyhow!("sftp.config_invalid: could not parse backend config: {e}")
        })?;
        Ok(cfg.normalized()?)
    }

    /// Render to the `accounts.backend_config_json` blob.
    pub fn to_json(&self) -> anyhow::Result<String> {
        serde_json::to_string(self).map_err(|e| {
            anyhow::anyhow!("sftp.config_invalid: could not serialize backend config: {e}")
        })
    }
}

/// The secret half of an SFTP account: a password, or a private key with an
/// optional passphrase. Lives in the OS keychain only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "auth", rename_all = "snake_case")]
pub enum SftpCredential {
    /// A plain SSH password.
    Password {
        /// The password.
        password: String,
    },
    /// An SSH private key, PEM-encoded, with an optional passphrase.
    PrivateKey {
        /// The PEM-encoded private key.
        pem: String,
        /// The passphrase protecting `pem`, if any.
        passphrase: Option<String>,
    },
}

// Hand-written so no field can ever reach a log line, a panic message, or the
// diagnostic bundle through a derived `Debug`. Redacts unconditionally
// (including an absent passphrase) so the shape alone never leaks anything
// about the secret's content.
impl std::fmt::Debug for SftpCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SftpCredential::Password { .. } => f
                .debug_struct("SftpCredential::Password")
                .field("password", &"<redacted>")
                .finish(),
            SftpCredential::PrivateKey { .. } => f
                .debug_struct("SftpCredential::PrivateKey")
                .field("pem", &"<redacted>")
                .field("passphrase", &"<redacted>")
                .finish(),
        }
    }
}

/// Keychain-backed store for one account's [`SftpCredential`].
///
/// One entry holding a serde-JSON blob of the credential, keyed by the
/// account id - the same pattern `driven_s3::config::S3CredentialStore` uses
/// for the S3 access key pair.
pub struct SftpCredentialStore {
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

impl SftpCredentialStore {
    /// A store for `account` (the `accounts.id` UUID string).
    pub fn new(account: impl Into<String>) -> Self {
        Self {
            account: account.into(),
        }
    }

    fn entry(&self) -> anyhow::Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_SFTP_CREDENTIALS_SERVICE, &self.account)
            .map_err(|e| anyhow::anyhow!("failed to open the SFTP credential keychain entry: {e}"))
    }

    /// Persist the credential, replacing any existing one.
    pub fn store(&self, cred: &SftpCredential) -> anyhow::Result<()> {
        let encoded = serde_json::to_string(cred)
            .map_err(|e| anyhow::anyhow!("failed to encode the SFTP credential: {e}"))?;
        keyring_off_runtime(|| {
            self.entry()?.set_password(&encoded).map_err(|e| {
                anyhow::anyhow!("failed to store the SFTP credential in the keychain: {e}")
            })
        })
    }

    /// Load the credential, or `None` when the account has none stored.
    pub fn load(&self) -> anyhow::Result<Option<SftpCredential>> {
        keyring_off_runtime(|| match self.entry()?.get_password() {
            Ok(raw) => Ok(Some(serde_json::from_str(&raw).map_err(|e| {
                anyhow::anyhow!(
                    "sftp.config_invalid: malformed SFTP credential keychain record: {e}"
                )
            })?)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!(
                "failed to read the SFTP credential from the keychain: {e}"
            )),
        })
    }

    /// Remove the credential. A missing entry is a no-op (idempotent, so
    /// account removal can call it unconditionally).
    pub fn purge(&self) -> anyhow::Result<()> {
        keyring_off_runtime(|| match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow::anyhow!(
                "failed to delete the SFTP credential from the keychain: {e}"
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(host: &str, root_path: &str, username: &str) -> SftpConfig {
        SftpConfig {
            host: host.to_string(),
            port: DEFAULT_PORT,
            root_path: root_path.to_string(),
            username: username.to_string(),
            auth: SftpAuthKind::Password,
            host_key_fingerprint: None,
        }
    }

    #[test]
    fn normalization_canonicalizes_host_username_and_root_path() {
        let c = cfg("  example.com  ", "/a/b//", "  user  ")
            .normalized()
            .expect("valid");
        assert_eq!(c.host, "example.com");
        assert_eq!(c.username, "user");
        // A trailing slash is stripped (unless the whole path is the bare
        // root) - it is a real filesystem path, not an S3-style key prefix
        // that needs one to concatenate cleanly.
        assert_eq!(c.root_path, "/a/b");
    }

    #[test]
    fn an_empty_root_path_normalizes_to_the_bare_root() {
        let c = cfg("example.com", "", "user").normalized().unwrap();
        assert_eq!(c.root_path, "/");
        let c = cfg("example.com", "   ", "user").normalized().unwrap();
        assert_eq!(c.root_path, "/");
    }

    #[test]
    fn the_bare_root_path_is_left_alone() {
        let c = cfg("example.com", "/", "user").normalized().unwrap();
        assert_eq!(c.root_path, "/");
    }

    #[test]
    fn invalid_configs_are_rejected() {
        assert_eq!(
            cfg("", "/", "user").normalized().unwrap_err(),
            SftpConfigError::Host
        );
        assert_eq!(
            cfg("example.com", "/", "").normalized().unwrap_err(),
            SftpConfigError::Username
        );
        assert_eq!(
            cfg("example.com", "/a/../../etc", "user")
                .normalized()
                .unwrap_err(),
            SftpConfigError::RootPath
        );
        assert_eq!(
            cfg("example.com", "/a\0b", "user")
                .normalized()
                .unwrap_err(),
            SftpConfigError::RootPath
        );
    }

    #[test]
    fn json_round_trips_and_applies_the_default_port() {
        let c = cfg("example.com", "/backups", "user").normalized().unwrap();
        let json = c.to_json().unwrap();
        assert_eq!(SftpConfig::from_json(&json).unwrap(), c);

        // A blob written without `port` (the minimum the UI can produce)
        // still loads, defaulting to DEFAULT_PORT.
        let minimal = SftpConfig::from_json(
            r#"{"host":"example.com","rootPath":"/backups","username":"user","auth":"password"}"#,
        )
        .expect("minimal config");
        assert_eq!(minimal.port, DEFAULT_PORT);
        assert_eq!(minimal.host_key_fingerprint, None);
    }

    #[test]
    fn the_config_blob_never_carries_credentials() {
        // Guard against someone adding a secret field to the persisted
        // config: it would land in SQLite, the diagnostic bundle, and any
        // backup of it.
        let json = cfg("example.com", "/backups", "user")
            .normalized()
            .unwrap()
            .to_json()
            .unwrap();
        // Match as a JSON KEY (`"field":`), not a value - `"auth":"password"`
        // is the (non-secret) auth-method tag and must not trip this guard.
        for forbidden in ["password", "passphrase", "privatekey", "pem", "secret"] {
            let key_pattern = format!("\"{forbidden}\":");
            assert!(
                !json.to_lowercase().contains(&key_pattern),
                "the persisted SFTP config must not contain a {forbidden:?} field: {json}"
            );
        }
    }

    #[test]
    fn password_credential_redacts_in_debug() {
        let cred = SftpCredential::Password {
            password: "hunter2-super-secret".to_string(),
        };
        let rendered = format!("{cred:?}");
        assert!(!rendered.contains("hunter2-super-secret"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn private_key_credential_redacts_pem_and_passphrase_in_debug() {
        let cred = SftpCredential::PrivateKey {
            pem: "-----BEGIN OPENSSH PRIVATE KEY-----\nsecretmaterial\n-----END OPENSSH PRIVATE KEY-----".to_string(),
            passphrase: Some("super-secret-passphrase".to_string()),
        };
        let rendered = format!("{cred:?}");
        assert!(!rendered.contains("secretmaterial"));
        assert!(!rendered.contains("super-secret-passphrase"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn credential_json_round_trips_both_variants() {
        for cred in [
            SftpCredential::Password {
                password: "p@ss/word".to_string(),
            },
            SftpCredential::PrivateKey {
                pem: "pem-bytes".to_string(),
                passphrase: Some("phrase".to_string()),
            },
            SftpCredential::PrivateKey {
                pem: "pem-bytes-no-passphrase".to_string(),
                passphrase: None,
            },
        ] {
            let json = serde_json::to_string(&cred).unwrap();
            let back: SftpCredential = serde_json::from_str(&json).unwrap();
            assert_eq!(back, cred);
        }
    }

    /// Isolate this test from the OS keychain. See
    /// `driven_test_fixtures::keychain` for why writing for real is not an
    /// acceptable fallback (on macOS it blocks the run on a modal permission
    /// prompt).
    fn keychain() -> Option<driven_test_fixtures::keychain::KeychainGuard> {
        driven_test_fixtures::keychain::isolated()
    }

    #[test]
    fn the_test_suite_is_isolated_from_the_os_keychain() {
        assert!(
            driven_test_fixtures::keychain::is_isolated(),
            "the in-memory keyring store must be the effective default store"
        );
    }

    #[test]
    fn credentials_round_trip_through_the_keychain() {
        let Some(_g) = keychain() else { return };
        let store = SftpCredentialStore::new("acct-sftp-round-trip");
        assert!(
            store
                .load()
                .expect("an empty store is not an error")
                .is_none(),
            "an account with no stored credential reads back as None, not an error"
        );

        let cred = SftpCredential::Password {
            password: "s3cr3t".to_string(),
        };
        store.store(&cred).expect("store");
        assert_eq!(store.load().expect("load"), Some(cred));

        // Rotation (e.g. switching from password to key auth) replaces
        // rather than appends.
        let rotated = SftpCredential::PrivateKey {
            pem: "new-pem".to_string(),
            passphrase: None,
        };
        store.store(&rotated).expect("rotate");
        assert_eq!(store.load().expect("load after rotate"), Some(rotated));
    }

    #[test]
    fn purging_credentials_is_idempotent() {
        // Account removal calls this unconditionally, including for accounts
        // that never had an SFTP credential, so a missing entry must not
        // error.
        let Some(_g) = keychain() else { return };
        let store = SftpCredentialStore::new("acct-sftp-purge");
        store.purge().expect("purging a missing entry is a no-op");

        store
            .store(&SftpCredential::Password {
                password: "secret".to_string(),
            })
            .expect("store");
        store.purge().expect("purge");
        assert!(store.load().expect("load after purge").is_none());
        store.purge().expect("a second purge is still a no-op");
    }

    #[test]
    fn credentials_are_scoped_per_account() {
        let Some(_g) = keychain() else { return };
        let a = SftpCredentialStore::new("acct-sftp-a");
        let b = SftpCredentialStore::new("acct-sftp-b");
        a.store(&SftpCredential::Password {
            password: "a-secret".to_string(),
        })
        .expect("store a");
        b.store(&SftpCredential::Password {
            password: "b-secret".to_string(),
        })
        .expect("store b");

        assert_eq!(
            a.load().unwrap(),
            Some(SftpCredential::Password {
                password: "a-secret".to_string()
            })
        );
        assert_eq!(
            b.load().unwrap(),
            Some(SftpCredential::Password {
                password: "b-secret".to_string()
            })
        );
        a.purge().expect("purge a");
        assert!(a.load().unwrap().is_none());
        assert!(b.load().unwrap().is_some(), "purging a must not touch b");
    }
}
