//! SFTP scenarios: the SSH backend against a REAL OpenSSH server, spawned as a
//! subprocess on a free localhost port (the MinIO/toxiproxy pattern from
//! `s3.rs` - there is no compose stack, the sidecar binaries are baked into the
//! e2e image).
//!
//! ## Why the server runs unprivileged, and what that costs
//!
//! `sshd` runs as the container's non-root `driven` user, which is what makes
//! the refused-write probe below mean anything: root bypasses the directory
//! mode bits, so a root-owned server would happily write into a `0555` root and
//! the assertion would prove nothing.
//!
//! The price is that PASSWORD auth is impossible here: verifying a password
//! needs `/etc/shadow` (or PAM), and both require root. So the e2e tier
//! authenticates with a KEY, and password auth stays covered where it can be
//! honest - the `driven-sftp` session unit tests and the chaos harness's
//! `TestSftpServer`, which implement the server side in-process.
//!
//! ## Credentials
//!
//! Nothing is baked into the image. Each scenario mints a throwaway ed25519
//! host key AND client keypair with `ssh-keygen` into its own tempdir, and both
//! die with the scenario.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use serde_json::Value;
use tokio::io::AsyncReadExt;

use crate::flows;
use crate::scenario::{Ctx, Scenario, Verdict};
use crate::session::{AppSession, Env, SessionConfig};

/// A private OpenSSH server for one scenario.
struct SftpStack {
    sshd: std::process::Child,
    /// The port sshd listens on (127.0.0.1 only).
    port: u16,
    /// The directory sshd serves, which the app uses as its destination root.
    /// The suite reads it DIRECTLY - the "remote" is the container's own
    /// filesystem, so the destination check needs no second protocol client.
    root: PathBuf,
    /// The PEM private key the app authenticates with.
    private_key: String,
    /// The local user sshd authenticates (the container's non-root `driven`).
    username: String,
    /// sshd's own log (`-E`); inlined into failure messages, because a
    /// container-only auth failure with no server log is near-undebuggable.
    log: PathBuf,
}

impl SftpStack {
    /// Generate the keys, write an sshd_config, boot sshd, and wait for the
    /// SSH identification string on the socket.
    async fn launch(work: &Path) -> anyhow::Result<Self> {
        let dir = work.join("sshd");
        let root = work.join("sftp-root");
        std::fs::create_dir_all(&dir)?;
        std::fs::create_dir_all(&root)?;

        let host_key = dir.join("host_ed25519");
        let client_key = dir.join("client_ed25519");
        keygen(&host_key)?;
        keygen(&client_key)?;
        let authorized_keys = dir.join("authorized_keys");
        std::fs::write(
            &authorized_keys,
            std::fs::read(dir.join("client_ed25519.pub"))?,
        )?;
        std::fs::set_permissions(&authorized_keys, std::fs::Permissions::from_mode(0o600))?;
        let private_key = std::fs::read_to_string(&client_key)?;

        let username = current_username()?;
        let port = free_port()?;
        let config = dir.join("sshd_config");
        // Absolute paths for everything: sshd re-execs itself per connection
        // and re-reads argv, so a relative path resolved against a different
        // cwd surfaces as an auth failure rather than a config error.
        std::fs::write(
            &config,
            format!(
                "ListenAddress 127.0.0.1\n\
                 HostKey {host_key}\n\
                 PidFile {pid}\n\
                 AuthorizedKeysFile {authorized}\n\
                 AllowUsers {username}\n\
                 PubkeyAuthentication yes\n\
                 PasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\n\
                 UsePAM no\n\
                 StrictModes no\n\
                 PrintMotd no\n\
                 X11Forwarding no\n\
                 AllowAgentForwarding no\n\
                 AllowTcpForwarding no\n\
                 Subsystem sftp internal-sftp\n\
                 LogLevel VERBOSE\n",
                host_key = host_key.display(),
                pid = dir.join("sshd.pid").display(),
                authorized = authorized_keys.display(),
            ),
        )?;

        let log = dir.join("sshd.log");
        let sshd_bin = sshd_binary().context("sshd is not in the image")?;
        let sshd = std::process::Command::new(&sshd_bin)
            .arg("-D")
            .arg("-f")
            .arg(&config)
            .arg("-p")
            .arg(port.to_string())
            .arg("-E")
            .arg(&log)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawning {}", sshd_bin.display()))?;

        let stack = Self {
            sshd,
            port,
            root,
            private_key,
            username,
            log,
        };
        wait_ssh_ready(port, Duration::from_secs(20))
            .await
            .with_context(|| format!("sshd never answered; log:\n{}", stack.log_tail()))?;
        Ok(stack)
    }

    /// The tail of sshd's log, for failure messages.
    fn log_tail(&self) -> String {
        let mut buf = String::new();
        if let Ok(mut f) = std::fs::File::open(&self.log) {
            let _ = f.read_to_string(&mut buf);
        }
        let start = buf.len().saturating_sub(2000);
        buf[start..].to_string()
    }
}

impl Drop for SftpStack {
    fn drop(&mut self) {
        let _ = self.sshd.kill();
        let _ = self.sshd.wait();
    }
}

/// Backup -> restore round trip against a real sshd through the app (SFTP
/// backend, keychain-stored key, real bytes over a real SSH socket), restored
/// BOTH ways: the production restore-job machinery in the GUI, and `driven-cli
/// restore --verify-against` standing alone against the app's persisted state.
pub struct SftpRoundTrip;

#[async_trait::async_trait]
impl Scenario for SftpRoundTrip {
    fn name(&self) -> &'static str {
        "sftp-round-trip"
    }
    fn description(&self) -> &'static str {
        "backup -> restore round trip against a real OpenSSH server, bytes compared (GUI + CLI)"
    }
    async fn run(&self, ctx: &Ctx) -> anyhow::Result<Verdict> {
        if sshd_binary().is_none() || which("ssh-keygen").is_none() {
            return Ok(Verdict::Skip("sshd/ssh-keygen not in the image".into()));
        }
        let work = tempfile::Builder::new()
            .prefix("driven-e2e-sftp-")
            .tempdir()?;
        let stack = SftpStack::launch(work.path()).await?;
        let src_dir = work.path().join("source");
        let restore_dir = work.path().join("restored");
        std::fs::create_dir_all(&src_dir)?;
        let rels = flows::seed_source_tree(&src_dir)?;

        let session = AppSession::launch(SessionConfig::default()).await?;
        let created = flows::create_sftp_account(
            &session,
            "127.0.0.1",
            stack.port,
            &stack.root,
            &stack.username,
            &stack.private_key,
        )
        .await
        .with_context(|| format!("creating the account; sshd log:\n{}", stack.log_tail()))?;

        // First contact must PIN a real host key (TOFU), and a fresh empty root
        // is STAMPED, not adopted.
        let fingerprint = created
            .get("hostKeyFingerprint")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let adopted = created.get("adopted").and_then(Value::as_bool);
        if !fingerprint.starts_with("SHA256:") || adopted != Some(false) {
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!(
                "first contact with a fresh root should pin a host key and stamp a \
                 marker: {created}"
            )));
        }

        let account = flows::account_id(&created)?;
        let source = flows::add_source(&session, &account, &src_dir).await?;
        flows::sync_now(&session, &source).await?;

        // Destination-observable wait: the objects must physically land under
        // the directory sshd serves.
        let waited = wait_for_dest_objects(&stack.root, rels.len(), Duration::from_secs(180)).await;
        session.screenshot(&ctx.artifacts, "01-after-sync").await?;
        if let Ok(landed) = &waited {
            // Logged, not just asserted: "PASS" alone cannot distinguish a real
            // upload from a scenario that skipped one.
            tracing::info!(
                objects = landed,
                fingerprint,
                "sftp destination populated over the wire"
            );
        }
        if let Err(e) = waited {
            let status = session.invoke("get_sync_status", Value::Null).await?;
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!(
                "the SFTP destination never reached {} objects: {e:#}; status={status}; \
                 sshd log:\n{}",
                rels.len(),
                stack.log_tail()
            )));
        }

        // Leg 1: restore through the production restore-job machinery.
        let rel_refs: Vec<&str> = rels.iter().map(String::as_str).collect();
        let job = flows::restore_and_wait(
            &session,
            &source,
            &rel_refs,
            &restore_dir,
            Duration::from_secs(180),
        )
        .await?;
        session
            .screenshot(&ctx.artifacts, "02-after-restore")
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

        // The Task 6 carry: a root the server cannot write to must fail clean.
        if let Some(why) = refused_write_persists_nothing(&session, &stack, work.path()).await? {
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(why));
        }

        // Leg 2: the GUI-free restore. Quit the app FIRST so the CLI stands
        // alone against the persisted state.db + the same OS keychain entry -
        // and so nothing contends on the database.
        let (data_dir, _data_guard) = session.quit_keep_data_dir().await?;
        let cli_dest = work.path().join("cli-restored");
        let out = std::process::Command::new(Env::cli_binary())
            .arg("restore")
            .arg("--db")
            .arg(data_dir.join("state.db"))
            .arg("--source-id")
            .arg(&source)
            .arg("--dest")
            .arg(&cli_dest)
            .arg("--verify-against")
            .arg(&src_dir)
            .output()
            .context("spawning driven-cli restore")?;
        if !out.status.success() {
            return Ok(Verdict::Fail(format!(
                "driven-cli restore --verify-against failed ({}):\nstdout:\n{}\nstderr:\n{}",
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        tracing::info!(
            report = %String::from_utf8_lossy(&out.stdout).trim().replace('\n', " | "),
            "driven-cli restored over SFTP from the app's own state + keychain"
        );
        // --verify-against only compares files the restore actually wrote, so
        // an empty restore would exit 0. Compare the trees ourselves too.
        let cli_mismatches = flows::compare_trees(&src_dir, &cli_dest)?;
        if !cli_mismatches.is_empty() {
            return Ok(Verdict::Fail(format!(
                "driven-cli round trip byte mismatch: {cli_mismatches:?}"
            )));
        }
        Ok(Verdict::Pass)
    }
}

/// Point a second account at a root the SERVER cannot write (mode `0555`, and
/// sshd is unprivileged so the bits bite): creation must fail ON THE WRITE, and
/// must persist nothing. `None` = the probe behaved; `Some(why)` = the failure
/// to report.
///
/// "On the write" is the part that needs guarding. `create_sftp_account`
/// rejects for many reasons before it ever tries to stamp a marker - a missing
/// root, a root that is not a directory, another account's marker - and a bare
/// `is_err()` would be satisfied by any of them, so a typo in the path would
/// keep this row green while testing nothing.
///
/// The error code cannot carry that weight: a read-only root currently surfaces
/// as `drive.unreachable` / "unclassified Drive error", which is
/// indistinguishable from a dead socket (a real finding, logged below and worth
/// classifying - but classification is not this row's to assert). So the proof
/// is a CONTROLLED EXPERIMENT instead: the same account, against the same path,
/// on the same server, is created twice - once at `0555` (must fail) and once
/// at `0755` (must succeed). The only variable is the permission bits, so the
/// refusal can only be the write being denied.
///
/// What is NOT asserted here: that the keychain entry is absent too. The
/// account list is the only surface this tier can see. Nor is that a
/// "rollback" - a refused probe never writes a credential in the first place,
/// because `create_sftp_account` probes BEFORE it persists anything. What the
/// unit tier asserts is that ORDERING guarantee (and the true rollback, which
/// only exists on the narrower path where the row write fails after the
/// keychain write succeeded).
async fn refused_write_persists_nothing(
    session: &AppSession,
    stack: &SftpStack,
    work: &Path,
) -> anyhow::Result<Option<String>> {
    let readonly = work.join("sftp-readonly-root");
    std::fs::create_dir_all(&readonly)?;
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o555))?;
    let before = account_count(session).await?;
    let refused = flows::create_sftp_account(
        session,
        "127.0.0.1",
        stack.port,
        &readonly,
        &stack.username,
        &stack.private_key,
    )
    .await;
    // Restore write permission IMMEDIATELY, before any further `?`: an error
    // path that left the directory read-only would fail the tempdir cleanup and
    // mask the real failure with a confusing secondary one.
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o755))?;

    match refused {
        Ok(dto) => {
            return Ok(Some(format!(
                "create_sftp_account SUCCEEDED against a read-only root: {dto}"
            )))
        }
        // Logged rather than pattern-matched - see the note above about
        // `drive.unreachable`.
        Err(e) => tracing::info!(error = %format!("{e:#}"), "read-only root refused creation"),
    }

    let after_refusal = account_count(session).await?;
    if after_refusal != before {
        return Ok(Some(format!(
            "a refused SFTP probe still persisted an account ({before} -> {after_refusal})"
        )));
    }

    // The control: same path, same server, write permission restored. If this
    // ALSO fails, the refusal above proved nothing about the permission bits.
    let allowed = flows::create_sftp_account(
        session,
        "127.0.0.1",
        stack.port,
        &readonly,
        &stack.username,
        &stack.private_key,
    )
    .await;
    match allowed {
        Ok(_) => {}
        Err(e) => {
            return Ok(Some(format!(
                "the read-only-root probe is not a permission test: the SAME root failed at 0755 \
                 too, so the earlier refusal had another cause: {e:#}"
            )))
        }
    }
    let after_control = account_count(session).await?;
    if after_control != before + 1 {
        return Ok(Some(format!(
            "the writable-root control did not persist exactly one account \
             ({before} -> {after_control})"
        )));
    }
    Ok(None)
}

/// How many accounts the app currently holds.
async fn account_count(session: &AppSession) -> anyhow::Result<usize> {
    let accounts = session.invoke("list_accounts", Value::Null).await?;
    Ok(accounts.as_array().map(Vec::len).unwrap_or(0))
}

/// Count DATA objects under the destination root: every control name this
/// backend writes (the `.driven-destination.json` marker, the
/// `.<stored>.driven-meta` sidecars, `.driven-tmp-*` staging files) starts with
/// a dot, and the seeded fixture has no dotfiles - so a plain file count would
/// reach the expected number while half the upload was still in flight.
fn count_dest_objects(root: &Path) -> anyhow::Result<usize> {
    if !root.is_dir() {
        return Ok(0);
    }
    let mut n = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
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

/// Poll until the destination root holds at least `n` data objects.
async fn wait_for_dest_objects(root: &Path, n: usize, timeout: Duration) -> anyhow::Result<usize> {
    let root = root.to_path_buf();
    crate::scenario::poll_until(timeout, || {
        let root = root.clone();
        async move {
            let count = count_dest_objects(&root)?;
            Ok(if count >= n { Some(count) } else { None })
        }
    })
    .await
}

/// Mint an unencrypted ed25519 keypair at `path` / `path.pub`.
fn keygen(path: &Path) -> anyhow::Result<()> {
    let status = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "driven-e2e", "-f"])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("running ssh-keygen")?;
    anyhow::ensure!(status.success(), "ssh-keygen failed for {}", path.display());
    Ok(())
}

/// The user this process runs as - the only account an unprivileged sshd can
/// authenticate, since it cannot change uid.
fn current_username() -> anyhow::Result<String> {
    let out = std::process::Command::new("id")
        .arg("-un")
        .output()
        .context("running id -un")?;
    anyhow::ensure!(out.status.success(), "id -un failed");
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Wait for sshd to answer with an SSH identification string. sshd speaks SSH,
/// not HTTP, so the S3 stack's `wait_http_ready` cannot be reused: it would
/// simply time out.
async fn wait_ssh_ready(port: u16, timeout: Duration) -> anyhow::Result<String> {
    let start = std::time::Instant::now();
    loop {
        if let Ok(mut sock) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            let mut buf = [0u8; 128];
            if let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf)).await
            {
                let greeting = String::from_utf8_lossy(&buf[..n]).trim().to_string();
                if greeting.starts_with("SSH-") {
                    return Ok(greeting);
                }
            }
        }
        if start.elapsed() > timeout {
            anyhow::bail!("timeout waiting for an SSH greeting on 127.0.0.1:{port}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The sshd binary. Checked by ABSOLUTE path first: it lives in `/usr/sbin`,
/// which is not on every non-root PATH, and a PATH miss here would silently
/// downgrade the scenario to a Skip that reads as a green suite.
fn sshd_binary() -> Option<PathBuf> {
    let sbin = PathBuf::from("/usr/sbin/sshd");
    if sbin.is_file() {
        return Some(sbin);
    }
    which("sshd")
}

/// Bind-then-drop free port.
fn free_port() -> anyhow::Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

/// Minimal PATH lookup.
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
