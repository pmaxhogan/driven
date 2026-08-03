//! Shared app flows the scenarios compose (account/source setup, sync waits,
//! restores) - all through the PRODUCTION IPC surface via [`AppSession::invoke`].
//!
//! The only non-production call is `e2e_pick_folder`, the env-gated headless
//! twin of the native folder picker (see `src-tauri/src/commands/e2e_hooks.rs`);
//! everything downstream of the minted dialog token is the real path.

use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Value};

use crate::scenario::poll_until;
use crate::session::AppSession;

/// Mint a dialog token for `path` via the env-gated e2e hook.
pub async fn pick_folder_token(session: &AppSession, path: &Path) -> anyhow::Result<String> {
    let picked = session
        .invoke(
            "e2e_pick_folder",
            json!({ "path": path.display().to_string() }),
        )
        .await?;
    picked
        .get("token")
        .and_then(Value::as_str)
        .map(String::from)
        .with_context(|| format!("e2e_pick_folder returned no token: {picked}"))
}

/// Create a local-folder destination account rooted at `dest_root`.
/// Returns the new account id.
pub async fn create_local_folder_account(
    session: &AppSession,
    dest_root: &Path,
) -> anyhow::Result<String> {
    let dto = session
        .invoke(
            "create_local_folder_account",
            json!({ "req": {
                "displayName": "e2e local folder",
                "root": dest_root.display().to_string(),
            }}),
        )
        .await?;
    dto.get("id")
        .and_then(Value::as_str)
        .map(String::from)
        .with_context(|| format!("create_local_folder_account returned no id: {dto}"))
}

/// Create an S3 destination account against a MinIO endpoint.
/// Returns the new account id.
pub async fn create_s3_account(
    session: &AppSession,
    endpoint: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
) -> anyhow::Result<String> {
    let dto = session
        .invoke(
            "create_s3_account",
            json!({ "req": {
                "displayName": "e2e minio",
                "endpoint": endpoint,
                "bucket": bucket,
                "region": "us-east-1",
                "pathStyle": true,
                "prefix": null,
                "accessKeyId": access_key,
                "secretAccessKey": secret_key,
            }}),
        )
        .await?;
    dto.get("id")
        .and_then(Value::as_str)
        .map(String::from)
        .with_context(|| format!("create_s3_account returned no id: {dto}"))
}

/// Create an SSH (SFTP) destination account against a live sshd, authenticating
/// with a PEM private key.
///
/// Unlike its siblings this returns the WHOLE `SftpAccountCreatedDto`, not just
/// the id: the pinned `hostKeyFingerprint` and the `adopted` flag are the two
/// things worth asserting about a first-contact SFTP probe, and they exist
/// nowhere else. Use [`account_id`] to pull the id back out.
pub async fn create_sftp_account(
    session: &AppSession,
    host: &str,
    port: u16,
    root_path: &Path,
    username: &str,
    private_key_pem: &str,
) -> anyhow::Result<Value> {
    session
        .invoke(
            "create_sftp_account",
            json!({ "req": {
                "displayName": "e2e sshd",
                "host": host,
                "port": port,
                "rootPath": root_path.display().to_string(),
                "username": username,
                "auth": "privateKey",
                "password": null,
                "privateKey": private_key_pem,
                "passphrase": null,
            }}),
        )
        .await
}

/// The account id inside an `SftpAccountCreatedDto`.
pub fn account_id(created: &Value) -> anyhow::Result<String> {
    created
        .pointer("/account/id")
        .and_then(Value::as_str)
        .map(String::from)
        .with_context(|| format!("no account id in {created}"))
}

/// Add `src_dir` as a backup source on `account_id` (destination = the
/// backend root). Returns the new source id.
pub async fn add_source(
    session: &AppSession,
    account_id: &str,
    src_dir: &Path,
) -> anyhow::Result<String> {
    add_source_to_folder(session, account_id, src_dir, "").await
}

/// [`add_source`] with an explicit destination folder id (the fake-Drive
/// scenarios browse the picker for a real fake-store folder id; the
/// localfs/S3 backends use the empty root id).
pub async fn add_source_to_folder(
    session: &AppSession,
    account_id: &str,
    src_dir: &Path,
    folder_id: &str,
) -> anyhow::Result<String> {
    let token = pick_folder_token(session, src_dir).await?;
    let dto = session
        .invoke(
            "add_source",
            json!({ "req": {
                "accountId": account_id,
                "displayName": "e2e source",
                "localPathToken": token,
                "localPath": src_dir.display().to_string(),
                "driveFolderId": folder_id,
                "driveId": null,
                "driveFolderPath": "/",
                "encryptionEnabled": false,
                "respectGitignore": false,
                "includePatterns": [],
                "excludePatterns": [],
            }}),
        )
        .await?;
    // AddSourceResult carries the created SourceDto (find the id wherever the
    // DTO nests it - `source.id` today; fall back to a top-level id).
    let id = dto
        .pointer("/source/id")
        .or_else(|| dto.get("id"))
        .and_then(Value::as_str)
        .map(String::from);
    id.with_context(|| format!("add_source returned no source id: {dto}"))
}

/// Trigger an out-of-band sync for `source_id` (gates bypassed - the harness
/// tests the engine through the app, not the power/metered gate logic, which
/// the chaos harness covers).
pub async fn sync_now(session: &AppSession, source_id: &str) -> anyhow::Result<()> {
    session
        .invoke(
            "sync_now",
            json!({ "sourceId": source_id, "bypassGates": true }),
        )
        .await?;
    Ok(())
}

/// Wait until `dir` contains at least `n` regular files (recursively) - the
/// destination-observable way to await a localfs-mirrored backup without
/// guessing at intermediate orchestrator states ("power_check", "scanning",
/// ...), which are not a stable wait surface.
pub async fn wait_for_dest_files(dir: &Path, n: usize, timeout: Duration) -> anyhow::Result<usize> {
    let dir = dir.to_path_buf();
    poll_until(timeout, || {
        let dir = dir.clone();
        async move {
            let count = count_files(&dir)?;
            Ok(if count >= n { Some(count) } else { None })
        }
    })
    .await
}

/// Count regular files under `dir` recursively (0 when the dir is missing).
pub fn count_files(dir: &Path) -> anyhow::Result<usize> {
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut n = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                n += 1;
            }
        }
    }
    Ok(n)
}

/// First page of the activity surface (the production IPC the Activity view
/// reads), newest first.
pub async fn activity_page(session: &AppSession) -> anyhow::Result<Value> {
    session
        .invoke(
            "query_activity",
            json!({ "filter": {}, "page": { "limit": 200 } }),
        )
        .await
}

/// Wait until the activity surface contains `needle` (case-insensitive, over
/// the raw page JSON). Returns the matching page for evidence.
pub async fn wait_for_activity(
    session: &AppSession,
    timeout: Duration,
    needle: &str,
) -> anyhow::Result<Value> {
    let needle = needle.to_lowercase();
    poll_until(timeout, || {
        let needle = needle.clone();
        async move {
            let page = activity_page(session).await?;
            Ok(if page.to_string().to_lowercase().contains(&needle) {
                Some(page)
            } else {
                None
            })
        }
    })
    .await
}

/// The wall-clock ms an account's circuit-breaker backoff window lifts, read
/// out of a `get_sync_status` payload; `None` when no account is parked in
/// `OrchestratorState::Backoff`.
///
/// The orchestrator's Drive circuit breaker is NOT bypassable: `sync_now`'s
/// `bypassGates` only opens the metered / battery / schedule gates, so a
/// scenario that cut the wire has to wait the breaker's window out rather than
/// re-trigger through it. The state serialises internally tagged on `state`
/// (snake_case), i.e. `{"state":"backoff","until":<epoch ms>}`.
///
/// Scans every account rather than matching an id: the harness runs one account
/// per session, and matching by id would bind the helper to the DTO's field
/// naming (`account_id` vs `accountId`) for no gain.
pub fn backoff_until(status: &Value) -> Option<i64> {
    status
        .get("accounts")?
        .as_array()?
        .iter()
        .filter_map(|account| {
            let state = account.get("state")?;
            if state.get("state").and_then(Value::as_str)? != "backoff" {
                return None;
            }
            state.get("until")?.as_i64()
        })
        .max()
}

/// Wall clock in Unix epoch ms - the same base the app's `Clock` stamps
/// `Backoff.until` with, so the two are directly comparable.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Restore `relative_paths` of `source_id` into `dest_dir` through the
/// production restore job machinery; waits for the job to finish and returns
/// the terminal job status JSON.
pub async fn restore_and_wait(
    session: &AppSession,
    source_id: &str,
    relative_paths: &[&str],
    dest_dir: &Path,
    timeout: Duration,
) -> anyhow::Result<Value> {
    std::fs::create_dir_all(dest_dir)?;
    let dest_token = pick_folder_token(session, dest_dir).await?;
    let items: Vec<Value> = relative_paths
        .iter()
        .map(|p| json!({ "sourceId": source_id, "relativePath": p }))
        .collect();
    let job = session
        .invoke(
            "restore_files",
            json!({ "items": items, "destToken": dest_token, "asOf": null }),
        )
        .await?;
    let job_id = job
        .as_str()
        .map(String::from)
        .or_else(|| job.get("jobId").and_then(Value::as_str).map(String::from))
        .with_context(|| format!("restore_files returned no job id: {job}"))?;

    let status = poll_until(timeout, || {
        let job_id = job_id.clone();
        async move {
            let s = session
                .invoke("get_restore_job", json!({ "job": job_id }))
                .await?;
            // TYPED terminal check: the DTO always carries the `done` /
            // `cancelled` KEYS, so substring matching on the raw JSON treated
            // every first poll as terminal (and `failedFiles:0` as a
            // failure) - parse the fields instead.
            let done = s.get("done").and_then(Value::as_bool).unwrap_or(false);
            let cancelled = s.get("cancelled").and_then(Value::as_bool).unwrap_or(false);
            Ok(if done || cancelled { Some(s) } else { None })
        }
    })
    .await?;
    Ok(status)
}

/// `true` when a terminal restore-job status reports every file restored
/// (zero failed files, no per-file error codes, not cancelled).
pub fn restore_job_clean(status: &Value) -> bool {
    let failed = status
        .get("failedFiles")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX);
    let cancelled = status
        .get("cancelled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let any_file_error = status
        .get("files")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .any(|f| f.get("errorCode").map(|e| !e.is_null()).unwrap_or(false))
        })
        .unwrap_or(true);
    failed == 0 && !cancelled && !any_file_error
}

/// Recursively byte-compare `restored` against `expected`. Returns the list of
/// mismatches (empty = identical).
pub fn compare_trees(expected: &Path, restored: &Path) -> anyhow::Result<Vec<String>> {
    let mut mismatches = Vec::new();
    let mut stack = vec![std::path::PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let exp = expected.join(&rel);
        for entry in std::fs::read_dir(&exp)? {
            let entry = entry?;
            let name = entry.file_name();
            let rel_child = rel.join(&name);
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(rel_child);
            } else if ft.is_file() {
                let got = restored.join(&rel_child);
                if !got.is_file() {
                    mismatches.push(format!("missing: {}", rel_child.display()));
                } else if std::fs::read(entry.path())? != std::fs::read(&got)? {
                    mismatches.push(format!("bytes differ: {}", rel_child.display()));
                }
            }
        }
    }
    Ok(mismatches)
}

/// Seed a small deterministic source tree (mixed sizes, a nested dir, a
/// unicode name) under `dir`; returns the relative paths written.
pub fn seed_source_tree(dir: &Path) -> anyhow::Result<Vec<String>> {
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("hello.txt", b"hello driven e2e".to_vec()),
        (
            "nested/deep/data.bin",
            (0u8..=255).cycle().take(64 * 1024 + 7).collect(),
        ),
        ("nested/empty.txt", Vec::new()),
        ("unicode-\u{00e9}\u{4e16}.md", b"# unicode name".to_vec()),
    ];
    let mut rels = Vec::new();
    for (rel, bytes) in files {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, bytes)?;
        rels.push(rel.to_string());
    }
    Ok(rels)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the `get_sync_status` wire shape [`backoff_until`] reads: the
    /// orchestrator state is internally tagged on a snake_case `state` key, so
    /// a backing-off account is `{"state":"backoff","until":<epoch ms>}`. A
    /// rename on the app side would otherwise make the heal phase silently
    /// blind to backoff again (issue #248).
    #[test]
    fn backoff_until_reads_the_tagged_state() {
        let status = json!({ "accounts": [
            { "account_id": "1", "state": { "state": "backoff", "until": 1_784_933_394_028_i64 } },
        ]});
        assert_eq!(backoff_until(&status), Some(1_784_933_394_028));
    }

    /// Every non-backoff state (and an empty account list) reads as "no
    /// backoff" - the heal loop must trigger a sync in those, not sit waiting.
    #[test]
    fn backoff_until_is_none_off_the_backoff_state() {
        for state in [
            json!({ "state": "idle", "last_run_at": null }),
            json!({ "state": "executing", "progress": { "files_done": 1 } }),
            json!({ "state": "paused", "reason": "offline" }),
        ] {
            let status = json!({ "accounts": [{ "account_id": "1", "state": state }] });
            assert_eq!(
                backoff_until(&status),
                None,
                "state must not read as backoff"
            );
        }
        assert_eq!(backoff_until(&json!({ "accounts": [] })), None);
        assert_eq!(backoff_until(&Value::Null), None);
    }

    /// Multi-account payloads take the LATEST deadline: waiting for the
    /// earliest would return to polling while another account is still gated.
    #[test]
    fn backoff_until_takes_the_latest_deadline() {
        let status = json!({ "accounts": [
            { "account_id": "1", "state": { "state": "backoff", "until": 10_i64 } },
            { "account_id": "2", "state": { "state": "idle", "last_run_at": null } },
            { "account_id": "3", "state": { "state": "backoff", "until": 99_i64 } },
        ]});
        assert_eq!(backoff_until(&status), Some(99));
    }
}
