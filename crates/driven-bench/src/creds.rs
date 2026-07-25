//! Credential loading and the destination safety rails.
//!
//! The suite talks to a REAL Google Drive account, so the rules here are
//! deliberately strict:
//!
//! - Credentials come from the environment only. Nothing is read from the OS
//!   keychain (so a bench run can never pick up the maintainer's personal
//!   account by accident) and nothing is ever printed.
//! - The destination folder must be named explicitly - by `--dest` or by
//!   `DRIVEN_E2E_DEST_FOLDER_ID`. There is no default and no discovery step.
//! - Every remote write goes under one freshly created run folder inside that
//!   destination, and cleanup trashes exactly that folder by the id it was
//!   created with. The suite never lists the destination and never matches by
//!   name, so it cannot delete anything it did not create.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use driven_drive::google::token_store::RefreshingTokenSource;
use driven_drive::google::GoogleDriveStore;
use driven_drive::remote_store::{DriveContext, RemoteStore};
use driven_drive::{CustomCaConfig, ProxyConfig};

/// Environment variable names, shared with the real-Drive e2e suite so one set
/// of secrets serves both (design/E2E_REAL.md).
pub const ENV_REFRESH_TOKEN: &str = "DRIVEN_E2E_REFRESH_TOKEN";
pub const ENV_DEST_FOLDER_ID: &str = "DRIVEN_E2E_DEST_FOLDER_ID";
pub const ENV_CLIENT_ID: &str = "DRIVEN_OAUTH_CLIENT_ID";
pub const ENV_CLIENT_SECRET: &str = "DRIVEN_OAUTH_CLIENT_SECRET";

/// The resolved credentials for a bench run. Deliberately has no `Debug` impl -
/// a stray `{:?}` is the classic way a token ends up in a log.
pub struct BenchCreds {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
}

impl BenchCreds {
    /// Reads the credentials from the environment.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            client_id: required(ENV_CLIENT_ID)?,
            client_secret: required(ENV_CLIENT_SECRET)?,
            refresh_token: required(ENV_REFRESH_TOKEN)?,
        })
    }

    /// Builds a live Drive store from the refresh token.
    ///
    /// Unlike the desktop app this never touches the keychain, so a rotated
    /// refresh token is not persisted anywhere - which is what we want for a
    /// throwaway benchmark identity.
    pub fn build_store(&self) -> Result<GoogleDriveStore> {
        let ca = CustomCaConfig::none();
        let proxy = ProxyConfig::system();
        let tokens = RefreshingTokenSource::from_stored_refresh_token(
            self.refresh_token.clone(),
            self.client_id.clone(),
            self.client_secret.clone(),
            &ca,
            &proxy,
        )
        .context("building the Drive token source from the refresh token")?;
        GoogleDriveStore::with_default_clients(tokens, &ca, &proxy)
            .context("building the Drive store")
    }
}

/// Reads one required environment variable, with an error that says how to fix
/// it rather than just naming the variable.
fn required(key: &str) -> Result<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ => anyhow::bail!(
            "{key} is not set. Source the gitignored .env.test at the repo root \
             (or set the four DRIVEN_* bench variables by hand); see bench/README.md."
        ),
    }
}

/// Resolves the destination folder id from an explicit flag or the environment.
///
/// This is a hard precondition, checked before a single byte is generated: a
/// benchmark that guessed its destination could write into a real backup.
pub fn resolve_dest_folder_id(explicit: Option<&str>) -> Result<String> {
    if let Some(id) = explicit {
        let id = id.trim();
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    required(ENV_DEST_FOLDER_ID).context(
        "the benchmark refuses to run without an explicit destination: pass --dest <folder id>",
    )
}

/// Loads `KEY=VALUE` pairs from a dotenv-style file, if it exists.
///
/// Existing environment variables always win, so CI secrets are never shadowed
/// by a stale local file. Values are not logged.
pub fn load_dotenv(path: &Path) -> Result<bool> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"').trim_matches('\'');
        if key.is_empty() || std::env::var_os(key).is_some() {
            continue;
        }
        // SAFETY-ish: this runs during single-threaded startup, before the
        // tokio runtime or any worker thread reads the environment.
        std::env::set_var(key, value);
    }
    Ok(true)
}

/// A remote folder created by the harness, which trashes itself on request.
///
/// Holding the id from creation is the entire safety story: cleanup never
/// enumerates, never searches by name, and therefore cannot touch anything that
/// was already in the destination.
pub struct RunFolder {
    store: Arc<dyn RemoteStore>,
    pub id: String,
    pub name: String,
}

impl RunFolder {
    /// Creates `name` under `parent_id`.
    pub async fn create(
        store: Arc<dyn RemoteStore>,
        parent_id: &str,
        name: String,
    ) -> Result<Self> {
        let entry = store
            .ensure_folder(parent_id, &name, &DriveContext::MyDrive)
            .await
            .with_context(|| format!("creating the run folder '{name}' under {parent_id}"))?;
        Ok(Self {
            store,
            id: entry.id,
            name,
        })
    }

    /// Creates a child folder under this one, for one scenario's uploads.
    pub async fn child(&self, name: &str) -> Result<String> {
        let entry = self
            .store
            .ensure_folder(&self.id, name, &DriveContext::MyDrive)
            .await
            .with_context(|| format!("creating scenario folder '{name}'"))?;
        Ok(entry.id)
    }

    /// Trashes this folder - and only this folder - by the id it was created
    /// with. Trashing a folder trashes its subtree, so one call cleans up every
    /// scenario beneath it. Already-gone is success (`trash` treats a 404 as
    /// the desired state).
    pub async fn cleanup(&self) -> Result<()> {
        self.store
            .trash(&self.id)
            .await
            .with_context(|| format!("trashing the run folder {} ({})", self.name, self.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn resolve_dest_prefers_the_explicit_flag() {
        assert_eq!(
            resolve_dest_folder_id(Some("folder-abc")).unwrap(),
            "folder-abc"
        );
    }

    #[test]
    fn resolve_dest_errors_when_nothing_is_set() {
        // The variable is intentionally absent in the unit-test environment.
        if std::env::var_os(ENV_DEST_FOLDER_ID).is_some() {
            return;
        }
        let err = resolve_dest_folder_id(None).unwrap_err().to_string();
        assert!(
            err.contains("--dest"),
            "the error must tell the user how to fix it, got: {err}"
        );
    }

    #[test]
    fn resolve_dest_treats_blank_as_unset() {
        if std::env::var_os(ENV_DEST_FOLDER_ID).is_some() {
            return;
        }
        assert!(resolve_dest_folder_id(Some("   ")).is_err());
    }

    #[test]
    fn dotenv_parses_pairs_and_skips_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env.test");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "# a comment").unwrap();
        writeln!(f).unwrap();
        writeln!(f, "DRIVEN_BENCH_DOTENV_PROBE=hello").unwrap();
        writeln!(f, "DRIVEN_BENCH_DOTENV_QUOTED=\"quoted value\"").unwrap();
        drop(f);

        std::env::remove_var("DRIVEN_BENCH_DOTENV_PROBE");
        std::env::remove_var("DRIVEN_BENCH_DOTENV_QUOTED");
        assert!(load_dotenv(&path).unwrap());
        assert_eq!(std::env::var("DRIVEN_BENCH_DOTENV_PROBE").unwrap(), "hello");
        assert_eq!(
            std::env::var("DRIVEN_BENCH_DOTENV_QUOTED").unwrap(),
            "quoted value"
        );
        std::env::remove_var("DRIVEN_BENCH_DOTENV_PROBE");
        std::env::remove_var("DRIVEN_BENCH_DOTENV_QUOTED");
    }

    #[test]
    fn dotenv_never_overrides_an_existing_variable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env.test");
        std::fs::write(&path, "DRIVEN_BENCH_DOTENV_WINS=from-file\n").unwrap();
        std::env::set_var("DRIVEN_BENCH_DOTENV_WINS", "from-env");
        load_dotenv(&path).unwrap();
        assert_eq!(
            std::env::var("DRIVEN_BENCH_DOTENV_WINS").unwrap(),
            "from-env",
            "a CI secret must never be shadowed by a stale local file"
        );
        std::env::remove_var("DRIVEN_BENCH_DOTENV_WINS");
    }

    #[test]
    fn dotenv_missing_file_is_not_an_error() {
        assert!(!load_dotenv(Path::new("no/such/.env.test")).unwrap());
    }
}
