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
//! M4 scaffold: the command tree compiles; each handler is `todo!()` for the
//! implement phase.

use clap::{Parser, Subcommand};

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
}

/// Arguments for `driven-cli auth`.
#[derive(Debug, clap::Args)]
struct AuthArgs {
    /// The OAuth client id (dev-only installed-app credential, SPEC s4).
    #[arg(long, env = "DRIVEN_OAUTH_CLIENT_ID")]
    client_id: String,
    /// The OAuth client secret (dev-only installed-app credential, SPEC s4).
    #[arg(long, env = "DRIVEN_OAUTH_CLIENT_SECRET")]
    client_secret: String,
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
    source: std::path::PathBuf,
    /// The destination Drive folder id to upload into.
    #[arg(long, env = "DRIVEN_E2E_DEST_FOLDER_ID")]
    dest_folder_id: String,
    /// The account whose stored refresh token authorizes the upload.
    #[arg(long)]
    account: String,
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
    }
}

/// Handler for `driven-cli auth` (SPEC s4 PKCE loopback flow).
async fn run_auth(args: AuthArgs) -> anyhow::Result<()> {
    let _ = args;
    todo!("M4 implement: run_pkce_loopback_flow + persist refresh token in keychain")
}

/// Handler for `driven-cli dump-refresh-token` (ROADMAP M4).
async fn run_dump_refresh_token(args: DumpRefreshTokenArgs) -> anyhow::Result<()> {
    let _ = args;
    todo!("M4 implement: read refresh token from KeyringTokenStore and print it")
}

/// Handler for `driven-cli sync` (ROADMAP M4 acceptance).
async fn run_sync(args: SyncArgs) -> anyhow::Result<()> {
    let _ = args;
    todo!("M4 implement: build GoogleDriveStore from stored token + run one sync cycle")
}
