//! `driven-cli restore` - restore one backup source to a local directory with
//! no GUI, and optionally byte-verify the result against the original folder.
//!
//! This is the terminal half of the round-trip harness: `driven-cli sync` (or
//! the app itself) puts bytes on the destination, and this command pulls them
//! back and proves they survived. The engine is
//! [`driven_core::restore_fetch`]; everything here is resolution and reporting -
//! find the source row, build the account's real store through the
//! `driven-backend` factory, unwrap the per-source key when the source is
//! encrypted, and render the outcome.
//!
//! It reads the SAME state database and the SAME OS-keychain secrets the app
//! uses, so it works against a real Google Drive / S3 / local-folder
//! destination without any extra configuration. The destination directory is
//! written with plain filesystem calls: unlike the GUI's restore, there is no
//! handle-based confinement, so point `--dest` somewhere you control.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

use driven_core::restore_fetch::{
    restore_source, FileOutcome, FileReport, RestoreOptions, RestoreReport,
};
use driven_core::state::{AccountRow, SourceRow, SqliteStateRepo, StateRepo};
use driven_core::types::{RelativePath, SourceId};
use driven_crypto::{DrivenCryptoSuite, Keystore, SourceCryptoSuite, WrappedSourceKey};

/// Args for `driven-cli restore`.
#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// Path to the Driven state database.
    #[arg(long, default_value = "state.db")]
    pub db: PathBuf,
    /// The source to restore, by `backup_sources.id`. Mutually exclusive with
    /// `--source-path`; when neither is given and the database holds exactly
    /// one source, that source is used.
    #[arg(long, conflicts_with = "source_path")]
    pub source_id: Option<String>,
    /// The source to restore, by its local root path (as configured).
    #[arg(long)]
    pub source_path: Option<PathBuf>,
    /// Directory to restore into. Created if absent; relative paths under the
    /// source root are preserved beneath it.
    #[arg(long)]
    pub dest: PathBuf,
    /// After restoring, byte-compare every restored file against the same
    /// relative path under this directory (normally the original source root).
    /// A mismatch or an unreadable counterpart makes the command exit non-zero;
    /// a file present here with no backup record at all is only NOTED, since
    /// that is what every scanner exclusion looks like.
    #[arg(long)]
    pub verify_against: Option<PathBuf>,
    /// Replace files that already exist under `--dest`. Off by default so a
    /// restore into a populated directory cannot silently clobber it.
    #[arg(long)]
    pub overwrite: bool,
    /// Do not abort when an ENCRYPTED source's key cannot be unwrapped.
    ///
    /// Off by default: no key means no plaintext, so the command fails closed
    /// before touching the destination. With this flag the run continues to the
    /// reporting stage and itemises every file as unrestorable (still exiting
    /// non-zero) - useful when a harness wants the standard summary shape rather
    /// than a bare error.
    #[arg(long)]
    pub allow_missing_crypto: bool,
}

/// Handler for `driven-cli restore`.
pub async fn run_restore(args: RestoreArgs) -> Result<()> {
    if !args.db.exists() {
        anyhow::bail!(
            "no state database at {} - point --db at Driven's state.db (is Driven configured?)",
            args.db.display()
        );
    }
    let state = SqliteStateRepo::open(&args.db)
        .await
        .with_context(|| format!("open state database {}", args.db.display()))?;

    let source = resolve_source(
        &state,
        args.source_id.as_deref(),
        args.source_path.as_deref(),
    )
    .await?;
    let account = resolve_account(&state, &source).await?;

    println!(
        "Restoring source '{}' ({}) from account {} into {}",
        source.display_name,
        source.id,
        account.email,
        args.dest.display()
    );

    // Per-source key material FIRST: an encrypted source with no key must fail
    // before a single object is downloaded.
    let suite = match resolve_crypto(&account, &source) {
        Ok(s) => s,
        Err(e) if args.allow_missing_crypto => {
            eprintln!("warning: {e}");
            let report = unrestorable_report(&state, &source, &format!("{e}")).await?;
            print_report(&report);
            anyhow::bail!("restore failed: the source's encryption key is unavailable");
        }
        Err(e) => return Err(e),
    };

    let store = build_remote_store(&account)?;

    let report = restore_source(
        &state,
        store.as_ref(),
        &source,
        suite.as_deref(),
        &args.dest,
        &RestoreOptions {
            overwrite: args.overwrite,
        },
    )
    .await?;
    print_report(&report);

    let verify = match args.verify_against.as_deref() {
        Some(original) => verify_against(&report, &args.dest, original)?,
        None => VerifyReport::default(),
    };

    if !report.ok() || verify.mismatches > 0 {
        anyhow::bail!(
            "restore failed: {} file(s) could not be restored, {} verification problem(s)",
            report.failed(),
            verify.mismatches
        );
    }
    Ok(())
}

/// Find the source row named by `--source-id` / `--source-path`, or the only
/// source there is. Any failure prints the available sources, because the whole
/// difficulty of running this command is knowing what to pass.
async fn resolve_source(
    state: &SqliteStateRepo,
    source_id: Option<&str>,
    source_path: Option<&Path>,
) -> Result<SourceRow> {
    let sources = state.list_sources().await.context("list sources")?;
    if sources.is_empty() {
        anyhow::bail!("the state database has no backup sources");
    }

    if let Some(id) = source_id {
        let wanted: SourceId = id
            .parse()
            .with_context(|| format!("--source-id {id} is not a UUID"))?;
        return sources
            .into_iter()
            .find(|s| s.id == wanted)
            .ok_or_else(|| anyhow::anyhow!("no source with id {wanted}"));
    }

    if let Some(path) = source_path {
        // Compare canonical forms when possible so `.` / `..` / a symlinked home
        // still match the stored absolute path; fall back to the literal string
        // when the path does not exist locally (a restore onto a new machine).
        let wanted = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut matches: Vec<SourceRow> = sources
            .into_iter()
            .filter(|s| {
                let stored = PathBuf::from(&s.local_path);
                std::fs::canonicalize(&stored).unwrap_or(stored) == wanted
            })
            .collect();
        return match matches.len() {
            1 => Ok(matches.remove(0)),
            0 => Err(anyhow::anyhow!("no source rooted at {}", path.display())),
            n => Err(anyhow::anyhow!(
                "{n} sources are rooted at {}; pass --source-id instead",
                path.display()
            )),
        };
    }

    if sources.len() == 1 {
        return Ok(sources.into_iter().next().unwrap_or_else(|| unreachable!()));
    }
    anyhow::bail!(
        "the database holds {} sources; pass --source-id or --source-path.\n{}",
        sources.len(),
        render_source_list(&sources)
    )
}

/// A `--source-id <id>  <name>  <path>` line per source, for an error message.
fn render_source_list(sources: &[SourceRow]) -> String {
    sources
        .iter()
        .map(|s| {
            format!(
                "  --source-id {}  {}  {}",
                s.id, s.display_name, s.local_path
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The account row that owns `source` (its destination + keychain identity).
async fn resolve_account(state: &SqliteStateRepo, source: &SourceRow) -> Result<AccountRow> {
    state
        .list_accounts()
        .await
        .context("list accounts")?
        .into_iter()
        .find(|a| a.id == source.account_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "source {} references account {}, which is not in the database",
                source.id,
                source.account_id
            )
        })
}

/// Build the account's REAL destination store through the shared
/// `driven-backend` factory, so Google Drive, S3 and local-folder accounts all
/// work and the secret comes from the same OS keychain entry the app uses.
///
/// There is deliberately no fake fallback: restoring from an in-memory store
/// would report success over bytes that do not exist.
fn build_remote_store(
    account: &AccountRow,
) -> Result<std::sync::Arc<dyn driven_drive::remote_store::RemoteStore>> {
    let ca = crate::cli_custom_ca();
    let proxy = crate::cli_proxy();
    let backend = driven_backend::AccountBackend {
        account_id: account.id.to_string(),
        kind: account.backend_kind,
        config_json: account.backend_config_json.clone(),
    };
    match driven_backend::build_store(
        &backend,
        driven_backend::BackendContext {
            ca: &ca,
            proxy: &proxy,
        },
    )? {
        driven_backend::StoreOutcome::Store(store) => Ok(store),
        driven_backend::StoreOutcome::NeedsReauth => anyhow::bail!(
            "account {} ({}) has no stored credential; re-authenticate it in Driven first",
            account.email,
            account.backend_kind
        ),
    }
}

/// Unwrap the per-source content key for an encrypted source: keystore ->
/// master key -> `WrappedSourceKey` -> `SourceKey` -> suite. Mirrors the app's
/// `KeystoreCryptoProvider` chain (which lives in `src-tauri` and is not
/// linkable from here).
///
/// FAILS CLOSED: any break in the chain is an error, never a silent plaintext
/// restore that would write ciphertext under the user's filenames. Returns
/// `Ok(None)` only for a source that is genuinely not encrypted.
fn resolve_crypto(
    account: &AccountRow,
    source: &SourceRow,
) -> Result<Option<Box<dyn SourceCryptoSuite>>> {
    if !source.encryption_enabled {
        return Ok(None);
    }
    let wrapped_bytes = source.wrapped_source_key.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "source {} is encrypted but has no wrapped_source_key stored",
            source.id
        )
    })?;
    let keystore = Keystore::open(&account.id.to_string()).with_context(|| {
        format!(
            "open the keystore for account {} (is the OS keychain unlocked?)",
            account.id
        )
    })?;
    let master_key = keystore
        .load_master_key()
        .with_context(|| format!("load the master key for account {}", account.id))?;
    let wrapped = WrappedSourceKey::from_bytes(wrapped_bytes)
        .map_err(|e| anyhow::anyhow!("the stored wrapped source key is malformed: {e}"))?;
    let source_key = master_key
        .unwrap_source_key(&wrapped)
        .map_err(|e| anyhow::anyhow!("unwrap the per-source key: {e}"))?;
    Ok(Some(Box::new(DrivenCryptoSuite::new(source_key))))
}

/// The report to print when an encrypted source's key is unavailable and
/// `--allow-missing-crypto` asked for the itemised form: every recorded file is
/// unrestorable, and saying so per file keeps the output shape identical to a
/// real run.
async fn unrestorable_report(
    state: &SqliteStateRepo,
    source: &SourceRow,
    reason: &str,
) -> Result<RestoreReport> {
    let rows = state
        .load_source_file_state(source.id)
        .await
        .context("load file_state")?;
    let mut paths: Vec<RelativePath> = rows.into_keys().collect();
    paths.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(RestoreReport {
        files: paths
            .into_iter()
            .map(|relative_path| FileReport {
                relative_path,
                outcome: FileOutcome::Failed(reason.to_string()),
            })
            .collect(),
    })
}

/// Print the per-file failures and skips, then the counts. Successes are
/// summarised rather than listed: a 100k-file restore should not bury its two
/// failures under 100k "ok" lines.
fn print_report(report: &RestoreReport) {
    for file in &report.files {
        match &file.outcome {
            FileOutcome::Restored { .. } => {}
            FileOutcome::Skipped(reason) => {
                println!("  skip   {} - {reason}", file.relative_path);
            }
            FileOutcome::Failed(reason) => {
                println!("  FAILED {} - {reason}", file.relative_path);
            }
        }
    }
    println!(
        "Restored {} file(s), {} byte(s); {} skipped, {} failed.",
        report.restored(),
        report.bytes_restored(),
        report.skipped(),
        report.failed()
    );
}

/// The outcome of `--verify-against`.
#[derive(Debug, Default, PartialEq, Eq)]
struct VerifyReport {
    /// Restored files whose bytes do NOT match the original (or whose original
    /// could not be read). These fail the command.
    mismatches: usize,
    /// Files under the original directory that the backup has no `file_state`
    /// row for at all. Reported, but NOT a failure - see [`verify_against`].
    untracked: usize,
}

/// Byte-compare the restored tree against `original`, and note anything under
/// `original` the backup has no row for at all.
///
/// Only a MISMATCH fails the command. The untracked count is deliberately
/// informational: a file the scanner excluded (a default exclusion, a
/// `.gitignore` rule when `respect_gitignore` is on, a user `exclude_pattern`)
/// never gets a `file_state` row, so gating on it would fail a perfectly correct
/// restore on the first `.DS_Store` and drown a repo-backed source in `.git/`
/// lines. Re-deriving the exclusion rules here would duplicate scanner logic
/// this command has no access to, and the rows the restore SKIPPED are already
/// reported by name, so the genuinely-silent gap is small and worth a note
/// rather than an exit code.
fn verify_against(report: &RestoreReport, dest: &Path, original: &Path) -> Result<VerifyReport> {
    let mut out = VerifyReport::default();

    for file in &report.files {
        if !matches!(file.outcome, FileOutcome::Restored { .. }) {
            continue;
        }
        let rel = file.relative_path.as_str();
        let restored = join_relative(dest, rel);
        let source = join_relative(original, rel);
        match files_differ(&restored, &source) {
            Ok(None) => {}
            Ok(Some(difference)) => {
                println!("  DIFF {rel} - {difference}");
                out.mismatches += 1;
            }
            Err(e) => {
                println!("  DIFF {rel} - could not compare: {e}");
                out.mismatches += 1;
            }
        }
    }

    let known: std::collections::HashSet<&str> = report
        .files
        .iter()
        .map(|f| f.relative_path.as_str())
        .collect();
    for path in walk_files(original) {
        let Ok(rel) = path.strip_prefix(original) else {
            continue;
        };
        let rel = rel
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if !known.contains(rel.as_str()) {
            println!("  note: {rel} is present locally but has no backup record (excluded?)");
            out.untracked += 1;
        }
    }

    if out.mismatches == 0 {
        println!(
            "Verified every restored file against {}.",
            original.display()
        );
    }
    Ok(out)
}

/// Compare two files without loading either whole into memory, returning
/// `Ok(None)` when they are identical and `Ok(Some(description))` when they are
/// not.
///
/// Chunked rather than `fs::read` on both sides: the restore engine streams to
/// disk precisely so a multi-GiB file never sits in RAM, and a verification step
/// that buffered both copies would make itself the peak-memory point of the
/// whole command.
fn files_differ(restored: &Path, original: &Path) -> Result<Option<String>> {
    let restored_len = std::fs::metadata(restored)
        .with_context(|| format!("stat {}", restored.display()))?
        .len();
    let original_len = std::fs::metadata(original)
        .with_context(|| format!("stat {}", original.display()))?
        .len();
    if restored_len != original_len {
        return Ok(Some(format!(
            "restored {restored_len} bytes, original {original_len} bytes"
        )));
    }

    let mut a = std::io::BufReader::new(
        std::fs::File::open(restored).with_context(|| format!("open {}", restored.display()))?,
    );
    let mut b = std::io::BufReader::new(
        std::fs::File::open(original).with_context(|| format!("open {}", original.display()))?,
    );
    let mut buf_a = vec![0u8; 64 * 1024];
    let mut buf_b = vec![0u8; 64 * 1024];
    let mut offset: u64 = 0;
    loop {
        // `read_exact`-style fills: a short `read` is not EOF, so compare only
        // the bytes both sides actually produced this round.
        let n_a = read_up_to(&mut a, &mut buf_a)?;
        let n_b = read_up_to(&mut b, &mut buf_b)?;
        if n_a == 0 && n_b == 0 {
            return Ok(None);
        }
        let n = n_a.min(n_b);
        if buf_a[..n] != buf_b[..n] {
            return Ok(Some(format!(
                "same length ({restored_len} bytes) but the contents differ near byte {offset}"
            )));
        }
        if n_a != n_b {
            // Equal metadata lengths but a different readable length: the file
            // changed underneath us.
            return Ok(Some(
                "the file changed while it was being verified".to_string(),
            ));
        }
        offset = offset.saturating_add(n as u64);
    }
}

/// Fill `buf` as far as the reader allows, returning the byte count (0 at EOF).
/// `Read::read` may return fewer bytes than requested without being at EOF, so a
/// bare `read` would make the two sides fall out of alignment.
fn read_up_to<R: std::io::Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(filled)
}

/// Join a `/`-separated relative path onto `root` one component at a time, so a
/// nested path is not handed to the OS as one long filename on Windows.
fn join_relative(root: &Path, rel: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for component in rel.split('/').filter(|c| !c.is_empty()) {
        out.push(component);
    }
    out
}

/// Every regular file under `root`, recursively. Errors are swallowed: an
/// unreadable subtree should not fail a verification whose real subject is the
/// files that ARE readable.
fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk_files(&path));
        } else if path.is_file() {
            out.push(path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use driven_core::restore_fetch::SkipReason;
    use driven_core::types::FileStateStatus;

    fn rel(s: &str) -> RelativePath {
        RelativePath::try_from(s.to_string()).expect("valid relative path")
    }

    fn restored(path: &str) -> FileReport {
        FileReport {
            relative_path: rel(path),
            outcome: FileOutcome::Restored {
                bytes: 0,
                from_bundle: false,
            },
        }
    }

    #[test]
    fn join_relative_splits_every_segment() {
        let joined = join_relative(Path::new("/tmp/x"), "a/b/c.txt");
        assert_eq!(
            joined,
            Path::new("/tmp/x").join("a").join("b").join("c.txt")
        );
        // Empty segments (a trailing or doubled slash) do not create empty
        // components.
        assert_eq!(
            join_relative(Path::new("/tmp/x"), "a//b/"),
            joined.parent().unwrap().to_path_buf()
        );
    }

    #[test]
    fn verify_reports_a_content_difference() {
        let dest = tempfile::tempdir().unwrap();
        let orig = tempfile::tempdir().unwrap();
        std::fs::write(dest.path().join("a.txt"), b"restored").unwrap();
        std::fs::write(orig.path().join("a.txt"), b"original").unwrap();
        let report = RestoreReport {
            files: vec![restored("a.txt")],
        };
        assert_eq!(
            verify_against(&report, dest.path(), orig.path()).unwrap(),
            VerifyReport {
                mismatches: 1,
                untracked: 0
            },
            "differing bytes must be reported"
        );
    }

    /// Same length, different contents - what a stat-only comparison waves
    /// through, and the reason the compare reads bytes rather than sizes.
    #[test]
    fn verify_catches_a_same_length_difference_past_the_first_chunk() {
        let dest = tempfile::tempdir().unwrap();
        let orig = tempfile::tempdir().unwrap();
        // Two chunks' worth, differing only in the very last byte, so a compare
        // that stopped after one 64 KiB buffer would miss it.
        let mut a = vec![9u8; 64 * 1024 * 2];
        let mut b = a.clone();
        *a.last_mut().unwrap() = 1;
        *b.last_mut().unwrap() = 2;
        std::fs::write(dest.path().join("big.bin"), &a).unwrap();
        std::fs::write(orig.path().join("big.bin"), &b).unwrap();
        let report = RestoreReport {
            files: vec![restored("big.bin")],
        };
        assert_eq!(
            verify_against(&report, dest.path(), orig.path())
                .unwrap()
                .mismatches,
            1
        );
    }

    /// A multi-chunk file that IS identical must verify clean - the guard
    /// against a chunked compare that falls out of alignment and cries wolf.
    #[test]
    fn verify_passes_on_a_large_identical_file() {
        let dest = tempfile::tempdir().unwrap();
        let orig = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = (0..(64 * 1024 * 3 + 77)).map(|i| (i % 251) as u8).collect();
        std::fs::write(dest.path().join("big.bin"), &bytes).unwrap();
        std::fs::write(orig.path().join("big.bin"), &bytes).unwrap();
        let report = RestoreReport {
            files: vec![restored("big.bin")],
        };
        assert_eq!(
            verify_against(&report, dest.path(), orig.path()).unwrap(),
            VerifyReport::default()
        );
    }

    #[test]
    fn verify_passes_on_identical_trees() {
        let dest = tempfile::tempdir().unwrap();
        let orig = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dest.path().join("sub")).unwrap();
        std::fs::create_dir_all(orig.path().join("sub")).unwrap();
        std::fs::write(dest.path().join("sub/a.txt"), b"same bytes").unwrap();
        std::fs::write(orig.path().join("sub/a.txt"), b"same bytes").unwrap();
        let report = RestoreReport {
            files: vec![restored("sub/a.txt")],
        };
        assert_eq!(
            verify_against(&report, dest.path(), orig.path()).unwrap(),
            VerifyReport::default()
        );
    }

    #[test]
    fn an_unrecorded_local_file_is_noted_but_does_not_fail_the_run() {
        // A file with no `file_state` row is worth surfacing, but it is NOT a
        // restore failure: the scanner's exclusions (defaults, `.gitignore`,
        // `exclude_patterns`) produce exactly this shape, so a `.DS_Store` or a
        // `.git/` tree must not turn a correct restore into a red exit code.
        let dest = tempfile::tempdir().unwrap();
        let orig = tempfile::tempdir().unwrap();
        std::fs::write(dest.path().join("a.txt"), b"same").unwrap();
        std::fs::write(orig.path().join("a.txt"), b"same").unwrap();
        std::fs::write(orig.path().join(".DS_Store"), b"excluded by default").unwrap();
        let report = RestoreReport {
            files: vec![restored("a.txt")],
        };
        assert_eq!(
            verify_against(&report, dest.path(), orig.path()).unwrap(),
            VerifyReport {
                mismatches: 0,
                untracked: 1
            }
        );
    }

    #[test]
    fn verify_does_not_flag_a_row_the_restore_deliberately_skipped() {
        // A never-uploaded row IS reported by the restore itself, so counting it
        // again as "untracked" would double-report a known gap.
        let dest = tempfile::tempdir().unwrap();
        let orig = tempfile::tempdir().unwrap();
        std::fs::write(orig.path().join("queued.txt"), b"not yet uploaded").unwrap();
        let report = RestoreReport {
            files: vec![FileReport {
                relative_path: rel("queued.txt"),
                outcome: FileOutcome::Skipped(SkipReason::NotUploaded),
            }],
        };
        assert_eq!(
            verify_against(&report, dest.path(), orig.path()).unwrap(),
            VerifyReport::default(),
            "a row the restore already reported must not be re-counted as untracked"
        );
    }

    #[test]
    fn skip_reasons_render_distinctly() {
        assert_ne!(
            SkipReason::NotUploaded.to_string(),
            SkipReason::NotSynced(FileStateStatus::Pending).to_string()
        );
        assert!(SkipReason::NotSynced(FileStateStatus::Corrupt)
            .to_string()
            .contains("Corrupt"));
    }
}
