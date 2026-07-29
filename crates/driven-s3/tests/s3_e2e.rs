//! End-to-end tests for [`S3Store`] against REAL S3-compatible servers.
//!
//! Two targets, each behind its own honest gate. These are NOT `#[ignore]`d -
//! an ignored test is invisible in CI output and reads as "passing"; a gated
//! test prints exactly why it did nothing and returns.
//!
//! 1. **Local MinIO** - runs whenever a `minio` binary is on `PATH`. The test
//!    spawns `minio server <tempdir>` on a free port with throwaway credentials,
//!    creates a bucket, runs the whole suite, and kills the server on the way
//!    out. Install with `brew install minio` (or the upstream binary).
//!
//! 2. **A remote S3-compatible service** (Cloudflare R2, AWS S3, ...) - runs
//!    when `DRIVEN_TEST_R2_S3_ENDPOINT`, `DRIVEN_TEST_R2_BUCKET`,
//!    `DRIVEN_TEST_R2_ACCESS_KEY_ID` and `DRIVEN_TEST_R2_SECRET_ACCESS_KEY` are
//!    all set (in the environment, or in a gitignored `.env.test` at the
//!    workspace root, which this harness will read if the vars are absent).
//!
//! Every run confines itself to a unique `driven-e2e-<nonce>/` prefix and
//! deletes every object it created, so a shared bucket stays clean even if
//! several runs overlap.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use driven_remote::remote_store::{
    DriveContext, RemoteStore, ResumableKind, ResumeProgress, UploadBody,
};
use driven_s3::{S3Config, S3Credentials, S3Store, PART_SIZE};
use driven_tls::{CustomCaConfig, ProxyConfig};
use md5::{Digest, Md5};

/// The wire chunk `driven-core`'s executor pushes at `resume_chunk`
/// (`executor.rs::WIRE_CHUNK`). Below S3's 5 MiB minimum part size on purpose:
/// the store must buffer these into legal parts, and the multipart test drives
/// exactly this chunking so the regression is caught here rather than on a
/// user's 5 GiB video.
const CORE_WIRE_CHUNK: usize = 4 * 1024 * 1024;

const ENV_ENDPOINT: &str = "DRIVEN_TEST_R2_S3_ENDPOINT";
const ENV_BUCKET: &str = "DRIVEN_TEST_R2_BUCKET";
const ENV_ACCESS_KEY: &str = "DRIVEN_TEST_R2_ACCESS_KEY_ID";
const ENV_SECRET_KEY: &str = "DRIVEN_TEST_R2_SECRET_ACCESS_KEY";
const ENV_REGION: &str = "DRIVEN_TEST_R2_REGION";

fn md5_of(bytes: &[u8]) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(bytes);
    h.finalize().into()
}

/// A per-run nonce, so concurrent runs against one bucket cannot collide.
fn nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}

/// Deterministic pseudo-random bytes, so a corrupted transfer cannot pass by
/// coincidence the way a buffer of zeroes might.
fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

// -- credential discovery -----------------------------------------------------

/// The workspace root, derived from this crate's manifest directory.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Read one `KEY=value` out of the gitignored `.env.test` at the workspace
/// root, so a local run needs no shell ceremony. Environment variables always
/// win; this is only consulted when a var is unset.
///
/// Deliberately minimal: no interpolation, no export handling, no quoting rules
/// beyond stripping a single pair of surrounding quotes.
fn from_dotenv(key: &str) -> Option<String> {
    let contents = std::fs::read_to_string(workspace_root().join(".env.test")).ok()?;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line.split_once('=')?;
        if k.trim() == key {
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(v);
            return Some(v.to_string());
        }
    }
    None
}

fn config_var(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| from_dotenv(key))
        .filter(|s| !s.is_empty())
}

// -- MinIO harness ------------------------------------------------------------

/// A locally spawned MinIO server, killed when this guard drops.
struct MinioServer {
    child: std::process::Child,
    endpoint: String,
    _dir: tempfile::TempDir,
}

impl Drop for MinioServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const MINIO_USER: &str = "drivene2e";
const MINIO_PASSWORD: &str = "drivene2esecret";

/// Pick a port the OS says is free right now.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Spawn MinIO, or `None` when the binary is not installed.
async fn spawn_minio() -> Option<MinioServer> {
    if std::process::Command::new("minio")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("skipping the MinIO S3 e2e: no `minio` binary on PATH (brew install minio)");
        return None;
    }

    let dir = tempfile::tempdir().expect("minio data dir");
    let port = free_port();
    let endpoint = format!("http://127.0.0.1:{port}");
    let child = std::process::Command::new("minio")
        .arg("server")
        .arg(dir.path())
        .arg("--address")
        .arg(format!("127.0.0.1:{port}"))
        .env("MINIO_ROOT_USER", MINIO_USER)
        .env("MINIO_ROOT_PASSWORD", MINIO_PASSWORD)
        // Silence the console/update chatter so a failing run's output is the
        // test's own.
        .env("MINIO_BROWSER", "off")
        .env("MINIO_UPDATE", "off")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn minio");

    let server = MinioServer {
        child,
        endpoint: endpoint.clone(),
        _dir: dir,
    };

    // Wait for readiness rather than sleeping a guessed interval.
    let health = format!("{endpoint}/minio/health/live");
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if let Ok(resp) = client.get(&health).send().await {
            if resp.status().is_success() {
                return Some(server);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("minio did not become healthy within 10s");
}

/// Create a bucket on a freshly spawned MinIO, using a signed request built the
/// same way the store builds its own.
async fn create_bucket(endpoint: &str, bucket: &str, creds: &S3Credentials) {
    use rusty_s3::{actions, Bucket, Credentials, S3Action, UrlStyle};
    let b = Bucket::new(
        endpoint.parse().expect("endpoint url"),
        UrlStyle::Path,
        bucket.to_string(),
        "us-east-1".to_string(),
    )
    .expect("bucket");
    let c = Credentials::new(creds.access_key_id.clone(), creds.secret_access_key.clone());
    let url = actions::CreateBucket::new(&b, &c).sign(Duration::from_secs(60));
    let resp = reqwest::Client::new()
        .put(url)
        .send()
        .await
        .expect("create bucket request");
    assert!(
        resp.status().is_success(),
        "creating the MinIO bucket failed: {}",
        resp.status()
    );
}

// -- the shared scenario suite ------------------------------------------------

/// Delete every object under `prefix`, so a shared bucket is left as it was
/// found. Best-effort but loud: a leak is reported rather than swallowed.
async fn cleanup(store: &S3Store, prefix: &str) {
    let ids = match store
        .list_folder(prefix, &DriveContext::MyDrive)
        .await
        .map(|entries| entries.into_iter().map(|e| e.id).collect::<Vec<_>>())
    {
        Ok(ids) => ids,
        Err(err) => {
            eprintln!("cleanup: could not list {prefix}: {err}");
            return;
        }
    };
    for id in ids {
        if id.ends_with('/') {
            Box::pin(cleanup(store, &id)).await;
        } else if let Err(err) = store.delete_permanent(&id).await {
            eprintln!("cleanup: could not delete an object: {err}");
        }
    }
}

fn props(source_id: &str, op_uuid: &str) -> HashMap<String, String> {
    let mut p = HashMap::new();
    p.insert(
        driven_remote::props::SOURCE_ID_KEY.to_string(),
        source_id.to_string(),
    );
    p.insert(
        driven_remote::props::CLIENT_OP_UUID_KEY.to_string(),
        op_uuid.to_string(),
    );
    p.insert(
        driven_remote::props::RELATIVE_PATH_HASH_KEY.to_string(),
        "deadbeef".to_string(),
    );
    p
}

async fn read_all(store: &S3Store, id: &str) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut stream = store.download(id).await.expect("download");
    let mut out = Vec::new();
    stream.0.read_to_end(&mut out).await.expect("read body");
    out
}

/// Install a tracing subscriber once, so the store's own warnings (the server's
/// reason for a failure the trait can only report as an opaque
/// `SessionInvalid`) reach the test output.
fn init_tracing() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("driven::s3=warn")),
            )
            .with_test_writer()
            .try_init();
    });
}

/// The full suite, run against whichever server the caller built the store for.
async fn run_suite(store: &S3Store, root: &str, label: &str) {
    init_tracing();
    eprintln!("[{label}] running the S3 backend suite under {root:?}");
    let source_id = format!("src-{}", nonce());

    // --- 1. small object round trip -----------------------------------------
    let small = payload(4096, 1);
    let entry = store
        .create(
            root,
            "small.bin",
            "application/octet-stream",
            UploadBody::Bytes(Bytes::from(small.clone())),
            props(&source_id, "op-small"),
        )
        .await
        .expect("create small");
    assert_eq!(entry.id, format!("{root}small.bin"));
    assert_eq!(entry.size, Some(small.len() as u64));
    assert_eq!(
        entry.md5,
        Some(md5_of(&small)),
        "[{label}] a single-PUT upload must return the server's ETag digest"
    );
    assert_eq!(
        entry
            .app_properties
            .get(driven_remote::props::SOURCE_ID_KEY)
            .map(String::as_str),
        Some(source_id.as_str()),
        "[{label}] app properties must survive the S3 metadata round trip"
    );
    assert_eq!(
        read_all(store, &entry.id).await,
        small,
        "[{label}] download"
    );

    // --- 2. folders are prefixes --------------------------------------------
    let folder = store
        .ensure_folder(root, "nested", &DriveContext::MyDrive)
        .await
        .expect("ensure_folder");
    assert_eq!(folder.id, format!("{root}nested/"));
    let nested_bytes = payload(1024, 2);
    let nested = store
        .create(
            &folder.id,
            "inner.bin",
            "application/octet-stream",
            UploadBody::Bytes(Bytes::from(nested_bytes.clone())),
            props(&source_id, "op-nested"),
        )
        .await
        .expect("create nested");

    let listed = store
        .list_folder(root, &DriveContext::MyDrive)
        .await
        .expect("list root");
    let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"small.bin"), "[{label}] got {names:?}");
    assert!(
        names.contains(&"nested"),
        "[{label}] the child prefix must list as a folder: {names:?}"
    );

    // --- 3. update overwrites in place --------------------------------------
    let updated_bytes = payload(8192, 3);
    let updated = store
        .update(
            &entry.id,
            UploadBody::Bytes(Bytes::from(updated_bytes.clone())),
            props(&source_id, "op-updated"),
        )
        .await
        .expect("update");
    assert_eq!(updated.id, entry.id, "[{label}] the key is the identity");
    assert_eq!(updated.md5, Some(md5_of(&updated_bytes)));
    assert_eq!(read_all(store, &updated.id).await, updated_bytes);

    // --- 4. reconciliation lookup by op uuid --------------------------------
    let found = store
        .find_by_op_uuid(root, "op-updated", &DriveContext::MyDrive)
        .await
        .expect("find_by_op_uuid");
    assert_eq!(
        found.map(|e| e.id),
        Some(entry.id.clone()),
        "[{label}] the crash-recovery lookup must find the object by its op uuid"
    );
    assert!(store
        .find_by_op_uuid(root, "op-that-never-ran", &DriveContext::MyDrive)
        .await
        .expect("find_by_op_uuid miss")
        .is_none());

    // --- 5. the remote-existence audit --------------------------------------
    let live = store
        .list_source_object_ids(&source_id, &DriveContext::MyDrive)
        .await
        .expect("list_source_object_ids");
    assert!(live.contains(&entry.id), "[{label}] live={live:?}");
    assert!(live.contains(&nested.id), "[{label}] live={live:?}");
    let other = store
        .list_source_object_ids("some-other-source", &DriveContext::MyDrive)
        .await
        .expect("audit for another source");
    assert!(
        !other.contains(&entry.id),
        "[{label}] the audit must not claim another source's objects"
    );

    // --- 6. multipart via the executor's resumable protocol -----------------
    // 20 MiB, pushed in the executor's exact 4 MiB wire chunks: that is BELOW
    // S3's 5 MiB minimum part size, so this fails outright unless the store
    // buffers chunks into parts.
    let big = payload(PART_SIZE * 2 + 4 * 1024 * 1024 + 12_345, 4);
    let big_md5 = md5_of(&big);
    let session = store
        .resumable_session(
            ResumableKind::Create {
                parent_id: root.to_string(),
                name: "big.bin".to_string(),
                app_properties: props(&source_id, "op-big"),
            },
            "application/octet-stream",
            big.len() as u64,
        )
        .await
        .expect("open a resumable session");

    let mut offset = 0usize;
    let mut completed = None;
    while offset < big.len() {
        let end = (offset + CORE_WIRE_CHUNK).min(big.len());
        let chunk = Bytes::copy_from_slice(&big[offset..end]);
        match store
            .resume_chunk(&session, offset as u64, chunk)
            .await
            .expect("resume_chunk")
        {
            ResumeProgress::InProgress { received } => {
                assert!(
                    received > offset as u64,
                    "[{label}] the session must make progress: at {offset}, got {received}"
                );
                offset = received as usize;
            }
            ResumeProgress::Completed(e) => {
                completed = Some(e);
                break;
            }
            ResumeProgress::SessionInvalid => {
                panic!(
                    "[{label}] the multipart session was invalidated mid-upload (run with \
                     RUST_LOG=driven::s3=warn for the server's reason)"
                )
            }
        }
    }
    let big_entry = completed.expect("the multipart upload must complete");
    assert_eq!(big_entry.size, Some(big.len() as u64));
    assert_eq!(
        big_entry.md5,
        Some(big_md5),
        "[{label}] the multipart digest must match the bytes sent"
    );
    assert_eq!(
        read_all(store, &big_entry.id).await,
        big,
        "[{label}] the reassembled multipart object must be byte-identical"
    );

    // A multipart object's ETag is a digest of digests, so `metadata` honestly
    // reports no content md5 rather than a plausible wrong one.
    let big_meta = store.metadata(&big_entry.id).await.expect("metadata");
    assert_eq!(
        big_meta.md5, None,
        "[{label}] a multipart object has no server-side content digest"
    );
    assert_eq!(
        big_meta
            .app_properties
            .get(driven_remote::props::SOURCE_ID_KEY)
            .map(String::as_str),
        Some(source_id.as_str()),
        "[{label}] multipart metadata must still carry the identity stamp"
    );

    // --- 7. multipart through the streaming create path ---------------------
    let streamed = payload(PART_SIZE + 1_000, 5);
    let streamed_md5 = md5_of(&streamed);
    let chunks: Vec<anyhow::Result<Bytes>> = streamed
        .chunks(CORE_WIRE_CHUNK)
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    let streamed_entry = store
        .create(
            root,
            "streamed.bin",
            "application/octet-stream",
            UploadBody::Stream {
                len: streamed.len() as u64,
                stream: Box::new(futures::stream::iter(chunks)),
            },
            props(&source_id, "op-streamed"),
        )
        .await
        .expect("streamed create");
    assert_eq!(streamed_entry.md5, Some(streamed_md5));
    assert_eq!(read_all(store, &streamed_entry.id).await, streamed);

    // --- 8. deletes are idempotent ------------------------------------------
    store.trash(&nested.id).await.expect("trash");
    store
        .trash(&nested.id)
        .await
        .expect("[{label}] deleting an already-deleted object must succeed");
    assert!(
        store.metadata(&nested.id).await.is_err(),
        "[{label}] a deleted object must not read back"
    );

    // --- 9. usage accounting -------------------------------------------------
    let about = store.about().await.expect("about");
    assert_eq!(about.limit, None, "[{label}] an S3 bucket has no quota");
    assert!(
        about.usage >= big.len() as u64,
        "[{label}] usage must count the objects we uploaded: {}",
        about.usage
    );

    eprintln!("[{label}] suite passed");
}

/// Build a store confined to a fresh prefix, run the suite, and clean up even if
/// the suite panics.
async fn run_against(config: S3Config, creds: S3Credentials, label: &str) {
    let root_prefix = format!("{}driven-e2e-{}/", config.root_prefix(), nonce());
    let scoped = S3Config {
        prefix: Some(root_prefix.clone()),
        ..config
    }
    .normalized()
    .expect("scoped config");
    let store = S3Store::new(
        &scoped,
        &creds,
        &CustomCaConfig::none(),
        &ProxyConfig::system(),
    )
    .expect("build the store");
    let root = store.root_id().to_string();

    let result = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(run_suite(
        &store, &root, label,
    )))
    .await;
    cleanup(&store, &root).await;
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

// -- the gated tests ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn minio_round_trip() {
    let Some(server) = spawn_minio().await else {
        return;
    };
    let creds = S3Credentials {
        access_key_id: MINIO_USER.to_string(),
        secret_access_key: MINIO_PASSWORD.to_string(),
    };
    let bucket = format!("driven-e2e-{}", nonce()).replace('.', "-");
    create_bucket(&server.endpoint, &bucket, &creds).await;

    run_against(
        S3Config {
            endpoint: server.endpoint.clone(),
            bucket,
            region: "us-east-1".to_string(),
            // MinIO's default deployment serves path style only.
            path_style: true,
            prefix: None,
        },
        creds,
        "minio",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_s3_round_trip() {
    let (Some(endpoint), Some(bucket), Some(access_key_id), Some(secret_access_key)) = (
        config_var(ENV_ENDPOINT),
        config_var(ENV_BUCKET),
        config_var(ENV_ACCESS_KEY),
        config_var(ENV_SECRET_KEY),
    ) else {
        eprintln!(
            "skipping the remote S3 e2e: set {ENV_ENDPOINT} + {ENV_BUCKET} + {ENV_ACCESS_KEY} + {ENV_SECRET_KEY} to run"
        );
        return;
    };

    run_against(
        S3Config {
            endpoint,
            bucket,
            // R2 accepts `auto` and `us-east-1` interchangeably; AWS needs the
            // real region, hence the override.
            region: config_var(ENV_REGION).unwrap_or_else(|| "auto".to_string()),
            // R2's account endpoint serves path style only.
            path_style: true,
            prefix: None,
        },
        S3Credentials {
            access_key_id,
            secret_access_key,
        },
        "remote",
    )
    .await;
}
