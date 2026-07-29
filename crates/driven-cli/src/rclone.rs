//! `driven-cli rclone` - bootstrap a Driven destination from an existing
//! `rclone.conf`.
//!
//! Two read-only subcommands:
//!
//! - `list` - every remote in the config and what Driven can make of it.
//! - `import <name>` - the settings for one remote, as text or `--json`.
//!
//! ## Why this does not create the account
//!
//! Creating a Driven destination probes the endpoint with the credentials before
//! persisting anything, rolls the keychain entry back if the row write fails,
//! and hot-spawns the account's orchestrator - all in `create_s3_account`. An
//! importer that wrote `accounts` rows itself would skip every one of those and,
//! with the desktop app running, write behind an already-loaded account cache.
//! So this command translates and the existing creation path still creates.
//!
//! ## Secrets
//!
//! `rclone.conf` is a file of credentials. Nothing here logs a value, and the
//! secret access key prints as `<redacted>` unless `--reveal-secrets` is passed
//! (whose help text says it writes a credential to stdout). The Google Drive
//! OAuth token is never printed at all, with or without the flag - it cannot be
//! used by Driven, so printing it would be pure exposure.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use driven_rclone::{
    classify, classify_all, locate_config, render_detail, render_list, to_json_pretty, ProcessEnv,
    RcloneConfig, RenderOptions,
};

/// Args for `driven-cli rclone`.
#[derive(Debug, Args)]
pub struct RcloneArgs {
    /// The operation to run.
    #[command(subcommand)]
    pub command: RcloneCommand,
}

/// The `driven-cli rclone` subcommands.
#[derive(Debug, Subcommand)]
pub enum RcloneCommand {
    /// List the remotes in an rclone config and what each maps to in Driven.
    List(ListArgs),
    /// Show the Driven settings for one rclone remote.
    Import(ImportArgs),
}

/// Args for `driven-cli rclone list`.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Path to the rclone config. Defaults to the locations rclone itself
    /// searches (`rclone config file` prints the one in use).
    #[arg(long)]
    pub config: Option<PathBuf>,
}

/// Args for `driven-cli rclone import`.
#[derive(Debug, Args)]
pub struct ImportArgs {
    /// The rclone remote name (the `[section]` header). Case-sensitive.
    pub remote: String,
    /// Path to the rclone config, as for `list`.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// The S3 bucket to back up into.
    ///
    /// rclone remotes carry no bucket - in rclone it is part of the path you
    /// type (`myremote:mybucket/dir`) - so this is the one setting that cannot
    /// come from the config file.
    #[arg(long)]
    pub bucket: Option<String>,
    /// An optional key prefix, to confine Driven to a subtree of the bucket.
    #[arg(long)]
    pub prefix: Option<String>,
    /// Emit JSON instead of text.
    #[arg(long)]
    pub json: bool,
    /// Print credential VALUES to stdout instead of `<redacted>`.
    ///
    /// Off by default because stdout ends up in scrollback, shell history via a
    /// redirect, and pasted bug reports. The Google Drive OAuth token is never
    /// printed even with this flag - it cannot authorize Driven.
    #[arg(long)]
    pub reveal_secrets: bool,
}

/// Load the config the user named, or the first one rclone would find,
/// returning it alongside the path it came from (which the renderer quotes so a
/// redacted secret is still findable).
fn load(explicit: Option<&PathBuf>) -> Result<(RcloneConfig, PathBuf)> {
    if let Some(path) = explicit {
        return Ok((RcloneConfig::read(path)?, path.clone()));
    }
    let found = locate_config(&ProcessEnv).map_err(|msg| anyhow::anyhow!(msg))?;
    // The PATH is safe to print; the contents are not.
    eprintln!(
        "Reading {} (found via {}).",
        found.path.display(),
        found.origin
    );
    Ok((RcloneConfig::read(&found.path)?, found.path))
}

/// Handler for `driven-cli rclone list`.
pub async fn run_list(args: ListArgs) -> Result<()> {
    let (config, _path) = load(args.config.as_ref())?;
    print!("{}", render_list(&classify_all(&config)));
    Ok(())
}

/// Handler for `driven-cli rclone import`.
pub async fn run_import(args: ImportArgs) -> Result<()> {
    let (config, path) = load(args.config.as_ref())?;
    let remote = config.remote(&args.remote).ok_or_else(|| {
        // Remote NAMES are not secret, so listing them is the most useful thing
        // a "no such remote" error can do.
        let known: Vec<&str> = config.remotes().iter().map(|r| r.name.as_str()).collect();
        if known.is_empty() {
            anyhow::anyhow!(
                "no remote named {:?}: this config has no remotes",
                args.remote
            )
        } else {
            anyhow::anyhow!(
                "no remote named {:?} (names are case-sensitive). This config has: {}",
                args.remote,
                known.join(", ")
            )
        }
    })?;

    let import = classify(remote);
    let opts = RenderOptions {
        bucket: args.bucket,
        prefix: args.prefix,
        reveal_secrets: args.reveal_secrets,
        config_path: Some(path.display().to_string()),
    };

    if args.json {
        println!("{}", to_json_pretty(&import, &opts));
    } else {
        print!("{}", render_detail(&import, &opts));
    }
    Ok(())
}
