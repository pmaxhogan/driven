//! `driven-cli` - a tiny debugging CLI for end-to-end Drive work without the
//! GUI (ROADMAP M4 acceptance).
//!
//! Subcommands:
//! - `auth` - run the SPEC s4 PKCE loopback flow, persisting the resulting
//!   refresh token in the OS keychain.
//! - `dump-refresh-token` - print the stored refresh token (so the
//!   maintainer can mint the `DRIVEN_E2E_REFRESH_TOKEN` used by the real-Drive
//!   contract suite; ROADMAP M4).
//! - `sync` - run one sync cycle against a real Drive folder (the M4
//!   acceptance "upload a 3-file test folder" path).
//!
//! The OAuth client id/secret come from a gitignored `client_secret.json` at
//! the repo root (the Google "installed app" download), or from
//! `--client-id` / `--client-secret` (env `DRIVEN_OAUTH_CLIENT_ID` /
//! `DRIVEN_OAUTH_CLIENT_SECRET`) when given. The public installed-app client
//! id is the default when none is supplied.
//!
//! The CLI deliberately uses ONLY the `driven-drive` public surface (plus
//! clap / tokio / anyhow / tracing) - it has no `reqwest` / `bytes` / `serde`
//! dependency of its own; those concerns live behind `driven-drive` helpers
//! (`UploadBytes`, `parse_installed_client_config`,
//! `RefreshingTokenSource::from_stored_refresh_token`, `md5_hex`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

mod inspect;

use driven_drive::google::oauth::{run_pkce_loopback_flow, OAuthProgress};
use driven_drive::google::token_store::{KeyringTokenStore, RefreshingTokenSource};
use driven_drive::google::{md5_hex, parse_installed_client_config, GoogleDriveStore, UploadBytes};
use driven_drive::remote_store::{RemoteStore, UploadBody};
use driven_drive::{CustomCaConfig, ProxyConfig};

/// The public installed-app client id (SPEC s4; M4 brief). Used when neither
/// `--client-id`, the env var, nor `client_secret.json` supplies one.
const DEFAULT_CLIENT_ID: &str =
    "1094503409775-kvuig3oqtchrq1s4tc1cnpi60mdvnqfe.apps.googleusercontent.com";

/// Driven debugging CLI (ROADMAP M4).
#[derive(Debug, Parser)]
#[command(name = "driven-cli", version, about)]
struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    command: Command,
}

/// The `driven-cli` subcommands (ROADMAP M4 acceptance).
#[derive(Debug, Subcommand)]
enum Command {
    /// Run the PKCE loopback OAuth flow and store the refresh token in the OS
    /// keychain (SPEC s4).
    Auth(AuthArgs),
    /// Print the stored refresh token for the authenticated account so it can
    /// be exported as `DRIVEN_E2E_REFRESH_TOKEN` (ROADMAP M4).
    DumpRefreshToken(DumpRefreshTokenArgs),
    /// Run one sync cycle of a local folder to a real Drive destination
    /// folder (ROADMAP M4 acceptance).
    Sync(SyncArgs),
    /// Show each backup source and its file-state counts from the local
    /// state database (no Drive / network access).
    Status(inspect::InspectArgs),
    /// Print recent activity-log entries from the local state database.
    History(inspect::HistoryArgs),
    /// Check the local state database for files in a corrupt / error state;
    /// exits non-zero when any are found.
    Verify(inspect::InspectArgs),
}

/// Arguments for `driven-cli auth`.
#[derive(Debug, clap::Args)]
struct AuthArgs {
    /// The account to store the refresh token under (keychain "username").
    #[arg(long)]
    account: String,
    /// The OAuth client id (dev-only installed-app credential, SPEC s4).
    /// Falls back to `client_secret.json` then the public default.
    #[arg(long, env = "DRIVEN_OAUTH_CLIENT_ID")]
    client_id: Option<String>,
    /// The OAuth client secret (dev-only installed-app credential, SPEC s4).
    /// Falls back to `client_secret.json`.
    #[arg(long, env = "DRIVEN_OAUTH_CLIENT_SECRET")]
    client_secret: Option<String>,
    /// Path to the Google "installed app" client config JSON (default:
    /// `client_secret.json` at the repo root).
    #[arg(long, default_value = "client_secret.json")]
    client_secret_file: PathBuf,
}

/// Arguments for `driven-cli dump-refresh-token`.
#[derive(Debug, clap::Args)]
struct DumpRefreshTokenArgs {
    /// The account whose stored refresh token to print (keychain lookup key).
    #[arg(long)]
    account: String,
}

/// Arguments for `driven-cli sync`.
#[derive(Debug, clap::Args)]
struct SyncArgs {
    /// The local folder to back up.
    #[arg(long)]
    source: PathBuf,
    /// The destination Drive folder id to upload into.
    #[arg(long, env = "DRIVEN_E2E_DEST_FOLDER_ID")]
    dest_folder_id: String,
    /// The Google Shared Drive id `dest_folder_id` lives in (issue #7). Omit
    /// (or pass "my-drive") for a My Drive destination; a Shared Drive id scopes
    /// the listing to that drive so a re-sync sees the existing objects.
    #[arg(long, env = "DRIVEN_E2E_SHARED_DRIVE_ID")]
    drive_id: Option<String>,
    /// The account whose stored refresh token authorizes the upload.
    #[arg(long)]
    account: String,
    /// OAuth client id (defaults as for `auth`).
    #[arg(long, env = "DRIVEN_OAUTH_CLIENT_ID")]
    client_id: Option<String>,
    /// OAuth client secret (defaults as for `auth`).
    #[arg(long, env = "DRIVEN_OAUTH_CLIENT_SECRET")]
    client_secret: Option<String>,
    /// Path to the Google "installed app" client config JSON.
    #[arg(long, default_value = "client_secret.json")]
    client_secret_file: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Auth(args) => run_auth(args).await,
        Command::DumpRefreshToken(args) => run_dump_refresh_token(args).await,
        Command::Sync(args) => run_sync(args).await,
        Command::Status(args) => inspect::run_status(args).await,
        Command::History(args) => inspect::run_history(args).await,
        Command::Verify(args) => inspect::run_verify(args).await,
    }
}

/// The resolved OAuth client credentials.
struct ClientCreds {
    client_id: String,
    client_secret: String,
}

/// Resolves the client id + secret, preferring explicit args/env, then the
/// `client_secret.json` file, with the public client id as a final default
/// for the id (the secret has no public default - a clear error if absent).
fn resolve_creds(
    client_id: Option<String>,
    client_secret: Option<String>,
    client_secret_file: &Path,
) -> anyhow::Result<ClientCreds> {
    // If both are supplied explicitly, use them as-is.
    if let (Some(id), Some(secret)) = (client_id.clone(), client_secret.clone()) {
        return Ok(ClientCreds {
            client_id: id,
            client_secret: secret,
        });
    }

    // Otherwise read the installed-app config file for the missing pieces.
    let file_creds = read_client_secret_file(client_secret_file)?;

    Ok(ClientCreds {
        client_id: client_id
            .or_else(|| file_creds.as_ref().map(|(id, _)| id.clone()))
            .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string()),
        client_secret: client_secret
            .or_else(|| file_creds.as_ref().map(|(_, secret)| secret.clone()))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no OAuth client secret: pass --client-secret / set DRIVEN_OAUTH_CLIENT_SECRET, \
                     or place the Google installed-app config at {}",
                    client_secret_file.display()
                )
            })?,
    })
}

/// Reads the installed-app `client_secret.json`, returning `None` if the file
/// is absent (so an explicit `--client-secret` can still satisfy the caller)
/// and an error only if it exists but cannot be parsed. Parsing is delegated
/// to `driven-drive` so the CLI needs no `serde` dependency.
fn read_client_secret_file(path: &Path) -> anyhow::Result<Option<(String, String)>> {
    match std::fs::read(path) {
        Ok(bytes) => parse_installed_client_config(&bytes)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("{} ({})", e, path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("failed to read {}: {e}", path.display())),
    }
}

/// Opens `url` in the system browser. Always prints the URL first as a
/// copy-paste fallback, then launches via a NO-SHELL per-OS launcher so the
/// URL's `&` query separators are never parsed by a command shell.
///
/// History: the previous Windows launcher used `cmd /C start "" <url>`. Despite
/// the empty title arg, `cmd.exe` parses `&` as a command separator BEFORE
/// `start` runs (Rust quotes args per the MSVCRT rules, which do NOT escape
/// `&` for cmd), so the browser only ever received the URL up to the first `&`
/// (dropping `scope`, `state`, `code_challenge`, ...) and Google returned
/// "Missing required parameter: scope". `rundll32` invokes no shell, so the
/// full URL (every `&`) reaches the default browser intact.
fn open_system_browser(url: &str) -> anyhow::Result<()> {
    // Surface the URL unconditionally so the user can always proceed by hand if
    // the auto-launch fails or opens the wrong application.
    println!("If your browser does not open, copy this URL into it:\n{url}\n");

    #[cfg(target_os = "windows")]
    let result = {
        // `rundll32 url.dll,FileProtocolHandler <url>` opens the default browser
        // without a command shell, so the URL's `&` separators pass through
        // literally (no cmd `&` command-splitting).
        std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn()
    };
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();

    match result {
        Ok(_child) => Ok(()),
        Err(e) => Err(anyhow::anyhow!(
            "could not launch a browser ({e}). Open the URL above manually."
        )),
    }
}

/// Handler for `driven-cli auth` (SPEC s4 PKCE loopback flow).
async fn run_auth(args: AuthArgs) -> anyhow::Result<()> {
    let creds = resolve_creds(args.client_id, args.client_secret, &args.client_secret_file)?;

    // Drain progress events on a background task so the flow can render the
    // consent step (the channel must keep up so `send().await` does not stall).
    let (tx, mut rx) = tokio::sync::mpsc::channel::<OAuthProgress>(8);
    let progress = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                OAuthProgress::OpeningBrowser => {
                    println!("Opening your browser to authorize Driven...");
                }
                OAuthProgress::WaitingForRedirect => {
                    println!("Waiting for you to approve access in the browser...");
                }
                OAuthProgress::ExchangingCode => {
                    println!("Exchanging the authorization code for tokens...");
                }
                OAuthProgress::Completed => {
                    println!("Authorization complete.");
                }
            }
        }
    });

    let tokens = run_pkce_loopback_flow(
        &creds.client_id,
        &creds.client_secret,
        open_system_browser,
        tx,
        &cli_custom_ca(),
        &cli_proxy(),
    )
    .await?;

    // Let the progress drainer finish.
    progress.await.ok();

    // Persist ONLY the refresh token (SPEC s4.1: access token stays in memory).
    let store = KeyringTokenStore::new(args.account.clone());
    store.store_refresh_token(&tokens.refresh_token)?;

    println!(
        "Stored the refresh token for account '{}' in the OS keychain.",
        args.account
    );
    println!(
        "You can now run `driven-cli sync` or mint DRIVEN_E2E_REFRESH_TOKEN via `dump-refresh-token`."
    );
    Ok(())
}

/// Handler for `driven-cli dump-refresh-token` (ROADMAP M4).
async fn run_dump_refresh_token(args: DumpRefreshTokenArgs) -> anyhow::Result<()> {
    let store = KeyringTokenStore::new(args.account.clone());
    match store.load_refresh_token()? {
        Some(token) => {
            // Print the bare token to stdout so it can be captured into an env
            // file (`DRIVEN_E2E_REFRESH_TOKEN=$(driven-cli dump-refresh-token ...)`).
            println!("{token}");
            Ok(())
        }
        None => Err(anyhow::anyhow!(
            "no refresh token stored for account '{}'; run `driven-cli auth --account {}` first",
            args.account,
            args.account
        )),
    }
}

/// Handler for `driven-cli sync` (ROADMAP M4 acceptance).
///
/// Builds a [`GoogleDriveStore`] from the stored refresh token and uploads
/// every file in `source` into the Drive `dest_folder_id`: an existing file
/// (matched by name) is updated by id, a new one is created. This is the thin
/// debug driver the ROADMAP M4 acceptance ("upload a 3-file test folder")
/// uses - it walks the dir and calls the store directly.
async fn run_sync(args: SyncArgs) -> anyhow::Result<()> {
    let creds = resolve_creds(args.client_id, args.client_secret, &args.client_secret_file)?;

    let store = build_store(&args.account, &creds)?;

    // Issue #7: scope every listing to the destination's drive (My Drive or a
    // Shared Drive) so a Shared-Drive dest does not list-empty and re-upload.
    let drive_context =
        driven_drive::remote_store::DriveContext::from_stored(args.drive_id.as_deref());

    // Map existing remote children (by name) so a re-sync UPDATES rather than
    // duplicates (Drive allows duplicate names; we must look up by name here
    // because this debug driver keeps no local state).
    let existing = store
        .list_folder(&args.dest_folder_id, &drive_context)
        .await?;
    let mut by_name: HashMap<String, String> = HashMap::new();
    for e in &existing {
        // First occurrence wins (oldest in the listing order).
        by_name
            .entry(e.name.clone())
            .or_insert_with(|| e.id.clone());
    }

    let mut uploaded = 0usize;
    let mut read_dir = tokio::fs::read_dir(&args.source)
        .await
        .map_err(|e| anyhow::anyhow!("failed to read source dir {}: {e}", args.source.display()))?;
    while let Some(entry) = read_dir
        .next_entry()
        .await
        .map_err(|e| anyhow::anyhow!("failed to enumerate source dir: {e}"))?
    {
        let path = entry.path();
        let file_type = entry.file_type().await?;
        if !file_type.is_file() {
            // V1 debug driver: top-level files only (no recursion).
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => {
                eprintln!("skipping non-UTF-8 filename: {}", path.display());
                continue;
            }
        };
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
        let mime = "application/octet-stream";
        let body = UploadBody::Bytes(UploadBytes::from(bytes));

        let result = if let Some(file_id) = by_name.get(&name) {
            println!("Updating '{name}' ({file_id})...");
            store.update(file_id, body, HashMap::new()).await?
        } else {
            println!("Creating '{name}'...");
            store
                .create(&args.dest_folder_id, &name, mime, body, HashMap::new())
                .await?
        };
        println!(
            "  -> {} ({} bytes, md5 {})",
            result.id,
            result.size.unwrap_or(0),
            result
                .md5
                .as_ref()
                .map(md5_hex)
                .unwrap_or_else(|| "-".to_string()),
        );
        uploaded += 1;
    }

    println!(
        "Synced {uploaded} file(s) into Drive folder {}.",
        args.dest_folder_id
    );
    Ok(())
}

/// Builds a live [`GoogleDriveStore`] from the keychain refresh token for
/// `account` (SPEC s4.1: refresh -> access token on demand). Shared by `sync`.
fn build_store(account: &str, creds: &ClientCreds) -> anyhow::Result<GoogleDriveStore> {
    // R-P2-1: wrap the keychain store in an Arc and wire it into the token
    // source via `.with_store(...)` so a refresh-token ROTATION (Google may
    // issue a new refresh token on a refresh) is PERSISTED back to the
    // keychain. Without this the rotated token lived only in memory and was
    // lost on restart, so the next `driven-cli sync` would re-use the stale
    // (possibly revoked) token and fail to authenticate.
    let store = std::sync::Arc::new(KeyringTokenStore::new(account.to_string()));
    let refresh_token = store.load_refresh_token()?.ok_or_else(|| {
        anyhow::anyhow!(
            "no refresh token stored for account '{account}'; run `driven-cli auth --account {account}` first"
        )
    })?;
    let ca = cli_custom_ca();
    let proxy = cli_proxy();
    let token_source = RefreshingTokenSource::from_stored_refresh_token(
        refresh_token,
        creds.client_id.clone(),
        creds.client_secret.clone(),
        &ca,
        &proxy,
    )?
    .with_store(store);
    GoogleDriveStore::with_default_clients(token_source, &ca, &proxy)
}

/// Issue #34: the dev/e2e CLI reads its custom root CA (if any) from the
/// `DRIVEN_CUSTOM_CA_PATH` env var so it can run behind the same corporate
/// TLS-inspecting proxy the desktop app supports via its settings. Unset =
/// system trust only (unchanged behaviour).
fn cli_custom_ca() -> CustomCaConfig {
    match std::env::var_os("DRIVEN_CUSTOM_CA_PATH") {
        Some(v) if !v.is_empty() => CustomCaConfig::from_path(Some(PathBuf::from(v))),
        _ => CustomCaConfig::none(),
    }
}

/// Issue #34: the dev/e2e CLI resolves its proxy from `DRIVEN_PROXY_URL` (an
/// `http`/`https`/`socks5`/`socks5h` URL). Unset = `System` mode, which honours
/// the standard `HTTP_PROXY`/`HTTPS_PROXY` env vars. PAC auto-config is a
/// desktop-app-only feature (it needs an async fetch); the dev CLI supports only
/// the env + manual forms. An invalid URL fails closed at client-build time.
fn cli_proxy() -> ProxyConfig {
    match std::env::var("DRIVEN_PROXY_URL") {
        Ok(url) if !url.trim().is_empty() => ProxyConfig::Manual(url.trim().to_string()),
        _ => ProxyConfig::system(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_custom_ca_reads_the_env_var() {
        // Issue #34: the dev CLI resolves its custom root CA from
        // DRIVEN_CUSTOM_CA_PATH (unset / blank = system trust only). No other
        // test touches this env var, so the set/remove here does not race.
        std::env::remove_var("DRIVEN_CUSTOM_CA_PATH");
        assert!(!cli_custom_ca().is_enabled(), "unset = system trust only");

        std::env::set_var("DRIVEN_CUSTOM_CA_PATH", "");
        assert!(!cli_custom_ca().is_enabled(), "blank = system trust only");

        std::env::set_var("DRIVEN_CUSTOM_CA_PATH", "/etc/corp/ca.pem");
        let ca = cli_custom_ca();
        assert!(ca.is_enabled());
        assert_eq!(ca.path(), Some(Path::new("/etc/corp/ca.pem")));

        std::env::remove_var("DRIVEN_CUSTOM_CA_PATH");
    }

    #[test]
    fn resolve_creds_prefers_explicit_args() {
        let creds = resolve_creds(
            Some("id-arg".to_string()),
            Some("secret-arg".to_string()),
            Path::new("/nonexistent.json"),
        )
        .unwrap();
        assert_eq!(creds.client_id, "id-arg");
        assert_eq!(creds.client_secret, "secret-arg");
    }

    #[test]
    fn resolve_creds_errors_without_secret_when_file_absent() {
        let r = resolve_creds(
            Some("id-arg".to_string()),
            None,
            Path::new("/definitely/not/here.json"),
        );
        assert!(r.is_err(), "missing secret with no file must error");
    }

    #[test]
    fn read_client_secret_file_parses_installed_shape() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("driven-cli-test-{}.json", unique_suffix()));
        std::fs::write(
            &path,
            br#"{"installed":{"client_id":"cid","client_secret":"csec","redirect_uris":["http://localhost"]}}"#,
        )
        .unwrap();
        let (id, secret) = read_client_secret_file(&path).unwrap().unwrap();
        assert_eq!(id, "cid");
        assert_eq!(secret, "csec");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_client_secret_file_absent_is_none() {
        let r = read_client_secret_file(Path::new("/definitely/not/here.json")).unwrap();
        assert!(r.is_none());
    }

    /// A tiny unique suffix for the temp file (no uuid dep in driven-cli).
    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }
}
