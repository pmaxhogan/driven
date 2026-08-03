//! S3 (MinIO) scenarios: a real network destination, with toxiproxy parked on
//! the wire so the suite can cut / heal the network mid-sync and assert the
//! app's honesty (error surfaced) and recovery (heal -> completes).
//!
//! Layout per scenario: `minio` listens on a private port, `toxiproxy-server`
//! proxies `app -> minio`, and the app's S3 account points at the PROXY. The
//! fault is injected at the toxiproxy admin API - a REAL TCP-level cut, not an
//! in-process fake.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Value};

use crate::flows;
use crate::scenario::{Ctx, Scenario, Verdict};
use crate::session::{wait_http_ready, AppSession, SessionConfig};

/// MinIO + toxiproxy stack for one scenario.
struct S3Stack {
    minio: std::process::Child,
    toxiproxy: std::process::Child,
    /// The app-facing endpoint (through the proxy).
    pub endpoint: String,
    /// toxiproxy admin API base.
    admin: String,
    client: reqwest::Client,
}

const ACCESS_KEY: &str = "driven-e2e";
const SECRET_KEY: &str = "driven-e2e-secret";
const BUCKET: &str = "driven-e2e";

impl S3Stack {
    /// Boot minio + toxiproxy, create the bucket, wire the proxy.
    async fn launch(work: &Path) -> anyhow::Result<Self> {
        let minio_port = free_port()?;
        let proxy_port = free_port()?;
        let admin_port = free_port()?;
        let data = work.join("minio-data");
        std::fs::create_dir_all(&data)?;

        let minio = std::process::Command::new("minio")
            .arg("server")
            .arg(&data)
            .arg("--address")
            .arg(format!("127.0.0.1:{minio_port}"))
            .env("MINIO_ROOT_USER", ACCESS_KEY)
            .env("MINIO_ROOT_PASSWORD", SECRET_KEY)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning minio (is it in the e2e image?)")?;
        let toxiproxy = std::process::Command::new("toxiproxy-server")
            .arg("--port")
            .arg(admin_port.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning toxiproxy-server")?;

        let admin = format!("http://127.0.0.1:{admin_port}");
        wait_http_ready(
            &format!("http://127.0.0.1:{minio_port}/minio/health/ready"),
            Duration::from_secs(20),
        )
        .await?;
        wait_http_ready(&format!("{admin}/version"), Duration::from_secs(20)).await?;

        let client = reqwest::Client::new();
        // Proxy: app -> 127.0.0.1:proxy_port -> minio.
        let resp = client
            .post(format!("{admin}/proxies"))
            .json(&json!({
                "name": "minio",
                "listen": format!("127.0.0.1:{proxy_port}"),
                "upstream": format!("127.0.0.1:{minio_port}"),
                "enabled": true,
            }))
            .send()
            .await?;
        anyhow::ensure!(
            resp.status().is_success(),
            "creating the toxiproxy proxy failed: {}",
            resp.status()
        );

        // Create the bucket with mc (the MinIO client shipped in the image).
        let mc_ok = std::process::Command::new("mc")
            .env(
                "MC_HOST_e2e",
                format!("http://{ACCESS_KEY}:{SECRET_KEY}@127.0.0.1:{minio_port}"),
            )
            .args(["mb", "--ignore-existing", &format!("e2e/{BUCKET}")])
            .status()
            .context("running mc mb")?;
        anyhow::ensure!(mc_ok.success(), "mc mb failed");

        Ok(Self {
            minio,
            toxiproxy,
            endpoint: format!("http://127.0.0.1:{proxy_port}"),
            admin,
            client,
        })
    }

    /// Throttle the app->minio upload stream to `rate_kbps` so a fixture
    /// upload demonstrably spans wall-clock time (a loopback MinIO otherwise
    /// swallows the whole fixture in under a second and the mid-sync cut
    /// lands after completion, proving nothing).
    async fn throttle_upload(&self, rate_kbps: u64) -> anyhow::Result<()> {
        let resp = self
            .client
            .post(format!("{}/proxies/minio/toxics", self.admin))
            .json(&json!({
                "name": "upload-bandwidth",
                "type": "bandwidth",
                "stream": "upstream",
                "toxicity": 1.0,
                "attributes": { "rate": rate_kbps },
            }))
            .send()
            .await?;
        anyhow::ensure!(
            resp.status().is_success(),
            "adding the bandwidth toxic failed: {}",
            resp.status()
        );
        Ok(())
    }

    /// Remove a named toxic (used to lift the throttle for the healed-retry
    /// phase).
    async fn remove_toxic(&self, name: &str) -> anyhow::Result<()> {
        let resp = self
            .client
            .delete(format!("{}/proxies/minio/toxics/{name}", self.admin))
            .send()
            .await?;
        anyhow::ensure!(
            resp.status().is_success(),
            "removing toxic {name} failed: {}",
            resp.status()
        );
        Ok(())
    }

    /// Hard-cut the wire (disable the proxy): every in-flight and new
    /// connection to the destination dies.
    async fn cut(&self) -> anyhow::Result<()> {
        let resp = self
            .client
            .post(format!("{}/proxies/minio", self.admin))
            .json(&json!({ "enabled": false }))
            .send()
            .await?;
        anyhow::ensure!(resp.status().is_success(), "toxiproxy cut failed");
        Ok(())
    }

    /// Heal the wire.
    async fn heal(&self) -> anyhow::Result<()> {
        let resp = self
            .client
            .post(format!("{}/proxies/minio", self.admin))
            .json(&json!({ "enabled": true }))
            .send()
            .await?;
        anyhow::ensure!(resp.status().is_success(), "toxiproxy heal failed");
        Ok(())
    }
}

impl Drop for S3Stack {
    fn drop(&mut self) {
        let _ = self.toxiproxy.kill();
        let _ = self.toxiproxy.wait();
        let _ = self.minio.kill();
        let _ = self.minio.wait();
    }
}

/// Backup -> restore round trip against MinIO through the app (S3 backend,
/// keychain-stored secret, real bytes over a real socket).
pub struct S3RoundTrip;

#[async_trait::async_trait]
impl Scenario for S3RoundTrip {
    fn name(&self) -> &'static str {
        "s3-round-trip"
    }
    fn description(&self) -> &'static str {
        "backup -> restore round trip against MinIO via the S3 backend, bytes compared"
    }
    async fn run(&self, ctx: &Ctx) -> anyhow::Result<Verdict> {
        if which("minio").is_none() || which("toxiproxy-server").is_none() {
            return Ok(Verdict::Skip("minio/toxiproxy not in PATH".into()));
        }
        let work = tempfile::Builder::new()
            .prefix("driven-e2e-s3-")
            .tempdir()?;
        let stack = S3Stack::launch(work.path()).await?;
        let src_dir = work.path().join("source");
        let restore_dir = work.path().join("restored");
        std::fs::create_dir_all(&src_dir)?;
        let rels = flows::seed_source_tree(&src_dir)?;

        let session = AppSession::launch(SessionConfig::default()).await?;
        let account =
            flows::create_s3_account(&session, &stack.endpoint, BUCKET, ACCESS_KEY, SECRET_KEY)
                .await?;
        let source = flows::add_source(&session, &account, &src_dir).await?;
        flows::sync_now(&session, &source).await?;
        // Await the upload by observing MinIO's on-disk keys (path-per-object
        // under <data>/<bucket>/): destination-observable, state-machine-free.
        let bucket_dir = work.path().join("minio-data").join(BUCKET);
        let waited =
            wait_for_minio_objects(&bucket_dir, rels.len(), Duration::from_secs(180)).await;
        session.screenshot(&ctx.artifacts, "01-after-sync").await?;
        if let Err(e) = waited {
            let status = session.invoke("get_sync_status", Value::Null).await?;
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!(
                "MinIO bucket never reached {} objects: {e:#}; status={status}",
                rels.len()
            )));
        }

        let rel_refs: Vec<&str> = rels.iter().map(String::as_str).collect();
        let job = flows::restore_and_wait(
            &session,
            &source,
            &rel_refs,
            &restore_dir,
            Duration::from_secs(180),
        )
        .await?;
        if !flows::restore_job_clean(&job) {
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!("restore job failed: {job}")));
        }
        let mismatches = flows::compare_trees(&src_dir, &restore_dir)?;
        if !mismatches.is_empty() {
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!("byte mismatch: {mismatches:?}")));
        }
        Ok(Verdict::Pass)
    }
}

/// Cut the wire mid-sync (toxiproxy disable), assert the app surfaces the
/// outage rather than claiming success; heal and assert the sync completes.
pub struct S3NetworkCutMidSync;

#[async_trait::async_trait]
impl Scenario for S3NetworkCutMidSync {
    fn name(&self) -> &'static str {
        "s3-network-cut-mid-sync"
    }
    fn description(&self) -> &'static str {
        "a TCP-level network cut mid-sync is surfaced, and healing lets the sync complete"
    }
    async fn run(&self, ctx: &Ctx) -> anyhow::Result<Verdict> {
        if which("minio").is_none() || which("toxiproxy-server").is_none() {
            return Ok(Verdict::Skip("minio/toxiproxy not in PATH".into()));
        }
        let work = tempfile::Builder::new()
            .prefix("driven-e2e-s3cut-")
            .tempdir()?;
        let stack = S3Stack::launch(work.path()).await?;
        let src_dir = work.path().join("source");
        std::fs::create_dir_all(&src_dir)?;
        // Enough bytes that a sync cannot finish instantly (32 files x 512 KiB).
        for i in 0..32 {
            let bytes: Vec<u8> = (0u8..=255).cycle().skip(i).take(512 * 1024).collect();
            std::fs::write(src_dir.join(format!("blob-{i:02}.bin")), bytes)?;
        }

        let session = AppSession::launch(SessionConfig::default()).await?;
        let account =
            flows::create_s3_account(&session, &stack.endpoint, BUCKET, ACCESS_KEY, SECRET_KEY)
                .await?;
        let source = flows::add_source(&session, &account, &src_dir).await?;
        // The bandwidth toxic is PER CONNECTION and the executor uploads many
        // files in parallel, so a generous per-connection rate still finished
        // the whole fixture before a t+2s cut (run 7). 32 KB/s per connection
        // caps even 16 parallel streams at ~512 KB/s aggregate: the ~16 MiB
        // fixture provably spans >30s and the cut lands mid-flight.
        stack.throttle_upload(32).await?;
        flows::sync_now(&session, &source).await?;

        // Cut the wire while the upload is in flight.
        tokio::time::sleep(Duration::from_secs(2)).await;
        stack.cut().await?;
        // PROVE the fault fired: the destination must NOT have all 32 blobs
        // (the cut landed mid-flight, not after completion).
        let bucket_dir = work.path().join("minio-data").join(BUCKET);
        let at_cut = count_minio_objects(&bucket_dir)?;
        if at_cut >= 32 {
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!(
                "all {at_cut} objects landed before the cut - fixture too small/fast to \
                 prove a mid-flight cut"
            )));
        }
        // The failure must SURFACE where users see it: the activity feed
        // carries error rows for the dead wire. The status DTO alone is not
        // the assertion surface - the orchestrator legitimately parks in
        // idle/backoff between retries.
        let surfaced = flows::wait_for_activity(&session, Duration::from_secs(60), "error").await;
        session.screenshot(&ctx.artifacts, "01-during-cut").await?;
        if let Err(e) = surfaced {
            let cut_status = session.invoke("get_sync_status", Value::Null).await?;
            let page = flows::activity_page(&session).await.unwrap_or(Value::Null);
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!(
                "wire cut mid-sync but no error surfaced on activity: {e:#}; \
                 status={cut_status}; page={page}"
            )));
        }

        // Heal; the engine's retry/backoff must finish the job - observed at
        // the destination (all 32 blobs present in MinIO's data dir).
        stack.heal().await?;
        // Lift the throttle so the healed retry completes promptly.
        let _ = stack.remove_toxic("upload-bandwidth").await;
        let healed = wait_out_backoff_for_objects(&session, &source, &bucket_dir, 32).await;
        session.screenshot(&ctx.artifacts, "02-after-heal").await?;
        if let Err(e) = healed {
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!(
                "healed wire but the destination never completed: {e:#}"
            )));
        }
        Ok(Verdict::Pass)
    }
}

/// Ceiling on the healed-retry phase (issue #248).
///
/// A hard TCP cut fails every in-flight request, and each failure past the
/// breaker's 5-failure threshold re-arms the window at the NEXT rung of
/// driven-core's `BACKOFF_SCHEDULE_MS` (30s, 1m, 2m, 5m, 10m, then plateau).
/// A parallel upload therefore saturates the schedule within one burst, and
/// the executor keeps retrying for a while after the wire is healed - so the
/// window can legitimately lift ~10 minutes after the LAST failed request,
/// not after the cut. Budget that plus the unthrottled re-upload.
const HEAL_CEILING: Duration = Duration::from_secs(16 * 60);

/// How often the heal phase re-triggers a sync while the account is NOT
/// backing off (a manual trigger coalesces, so this cannot stack cycles).
const HEAL_TRIGGER_EVERY: Duration = Duration::from_secs(20);

/// Destination poll cadence during the heal phase: short enough that a wait
/// on a distant backoff deadline is still tracked (progress is observed every
/// slice, never slept through blind).
const HEAL_POLL_SLICE: Duration = Duration::from_secs(2);

/// Wait for the healed sync to land `want` objects in MinIO, staying aware of
/// the orchestrator's circuit-breaker backoff (issue #248).
///
/// `sync_now(bypassGates)` opens the metered / battery / schedule gates only -
/// it deliberately does NOT bypass the Drive circuit breaker - so after a hard
/// cut the account parks in `Backoff{until}` and a fire-and-poll heal phase
/// just watches a fixed timer expire. This instead reads `until` out of the
/// live `get_sync_status` payload and waits it out, re-triggering a sync
/// whenever the account is not gated (an already-expired `until` still reads
/// as `Backoff` until the next gate evaluation rewrites the state, so an
/// elapsed deadline falls through to the trigger branch rather than parking).
async fn wait_out_backoff_for_objects(
    session: &AppSession,
    source: &str,
    bucket_dir: &std::path::Path,
    want: usize,
) -> anyhow::Result<usize> {
    let start = std::time::Instant::now();
    let mut last_trigger: Option<std::time::Instant> = None;
    let mut saw_backoff = false;
    let mut logged_until: Option<i64> = None;
    let mut status = Value::Null;
    loop {
        let count = count_minio_objects(bucket_dir)?;
        if count >= want {
            return Ok(count);
        }
        if start.elapsed() > HEAL_CEILING {
            let still_gated = flows::backoff_until(&status).is_some();
            anyhow::bail!(
                "destination stalled at {count}/{want} objects after {:?} (backoff observed: \
                 {saw_backoff}; still reporting backoff at timeout: {still_gated}); \
                 status={status}",
                start.elapsed()
            );
        }
        status = session.invoke("get_sync_status", Value::Null).await?;
        let remaining_ms = flows::backoff_until(&status).map(|until| until - flows::now_ms());
        match remaining_ms {
            Some(remaining) if remaining > 0 => {
                saw_backoff = true;
                // Log each DISTINCT deadline (a re-open moves it), so the run
                // log shows how long the phase waited and why.
                let until = flows::backoff_until(&status);
                if logged_until != until {
                    logged_until = until;
                    tracing::info!(
                        remaining_secs = remaining / 1000,
                        "heal phase: account is in circuit-breaker backoff; waiting it out"
                    );
                }
                let wait = Duration::from_millis(remaining as u64).min(HEAL_POLL_SLICE);
                tokio::time::sleep(wait).await;
            }
            _ => {
                if last_trigger.is_none_or(|t| t.elapsed() >= HEAL_TRIGGER_EVERY) {
                    flows::sync_now(session, source).await?;
                    last_trigger = Some(std::time::Instant::now());
                }
                tokio::time::sleep(HEAL_POLL_SLICE).await;
            }
        }
    }
}

/// Count OBJECTS in a MinIO data dir subtree: one `xl.meta` per object
/// (MinIO stores each key as a directory holding xl.meta + part files, so a
/// plain file count over-reports).
fn count_minio_objects(bucket_dir: &std::path::Path) -> anyhow::Result<usize> {
    if !bucket_dir.is_dir() {
        return Ok(0);
    }
    let mut n = 0usize;
    let mut stack = vec![bucket_dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() && entry.file_name() == "xl.meta" {
                n += 1;
            }
        }
    }
    Ok(n)
}

/// Poll until the MinIO data dir holds at least `n` objects.
async fn wait_for_minio_objects(
    bucket_dir: &std::path::Path,
    n: usize,
    timeout: Duration,
) -> anyhow::Result<usize> {
    let dir = bucket_dir.to_path_buf();
    crate::scenario::poll_until(timeout, || {
        let dir = dir.clone();
        async move {
            let count = count_minio_objects(&dir)?;
            Ok(if count >= n { Some(count) } else { None })
        }
    })
    .await
}

/// Bind-then-drop free port.
fn free_port() -> anyhow::Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

/// Minimal PATH lookup.
fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
