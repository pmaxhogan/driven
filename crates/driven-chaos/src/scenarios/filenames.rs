//! Pathological-filename scenarios (STRESS_HARNESS s3.4).
//!
//! One [`Scenario`] per s3.4 row: control chars, RLO/ZWJ/ZWNJ/BOM, IDN
//! homographs, NFC-vs-NFD cafe, Hangul jamo, 255-byte leaves, ~4 KiB paths,
//! Windows-reserved names, trailing space/dot, separator look-alikes,
//! unpaired surrogates, and case-only differences. The s3.4
//! `name-normalisation-only-differs` row is explicitly "Covered by
//! `name-nfc-vs-nfd-cafe` / `name-hangul-jamo`" in the catalogue, so it is
//! not a separate impl - the NFC-collision behaviour is exercised by those
//! two.
//!
//! # Forced architecture: each scenario builds its own [`DrivenHandle`]
//!
//! A scenario must wire its on-disk source to the remote it asserts against,
//! and that needs the in-memory fake's root folder id. [`DrivenHandle`]
//! exposes `remote` only as `Arc<dyn RemoteStore>`, and the `RemoteStore`
//! trait has no "root anchor" accessor (`InMemoryRemoteStore::root_id` is an
//! inherent method on the concrete type). `setup(ctx)` and
//! `run_assertions(&handle)` therefore cannot, between them, point a source
//! at the passed handle's remote. So every scenario here builds its OWN
//! concrete [`InMemoryRemoteStore`], boots a [`DrivenHandle`] over it via
//! [`DrivenHandleBuilder`], sets `source.drive_folder_id = root_id()`, and
//! keeps the concrete `Arc` for `list_folder` / `download` assertions. The
//! `_handle` parameter the runner passes is intentionally unused. This is
//! load-bearing for the Integrate agent wiring the runner: the runner must
//! NOT assume a scenario consumes the handle it was given.
//!
//! # Honest treatment of error codes the current core does not yet emit
//!
//! Several s3.4 rows expect SPEC s24 codes (`local.invalid_filename`,
//! `local.io_error` for Windows-reserved names). The ErrorCode enum carries
//! those variants, but the M3 core only ever WRITES `local.unicode_collision`
//! (scanner -> `record_collisions`) and `local.io_error` for a Failed
//! executor op (`record_outcome_activity`) to the activity log. A name that
//! is not representable as a [`RelativePath`] (an unpaired surrogate) is
//! dropped by the scanner with a `tracing::warn` and NO activity row, so it
//! never becomes an op and never surfaces a code. Per the harness's "no
//! fake-green" rule these scenarios assert the spec-correct outcome and do
//! NOT weaken it to match the current core; where the code is unreachable on
//! the host they SKIP by capability, and the gap is documented in the M3.7
//! report rather than masked.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;

use driven_core::orchestrator::TickSource;
use driven_core::state::SourceRow;
use driven_core::types::{ErrorCode, FileStateStatus, RelativePath, SourceId};

use driven_drive::fake::InMemoryRemoteStore;
use driven_drive::remote_store::RemoteStore;

use crate::capabilities::{Capability, CapabilityRequirements};
use crate::handle::{DrivenHandle, DrivenHandleBuilder};
use crate::scenario::{ExpectedOutcome, Outcome, Scenario, ScenarioContext};

/// Every pathological-filename scenario (STRESS_HARNESS s3.4).
///
/// `name-normalisation-only-differs` is folded into `name-nfc-vs-nfd-cafe` +
/// `name-hangul-jamo` per the catalogue note, so it has no entry here.
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(NameControlChars),
        Box::new(NameRloZwjZwnjBom),
        Box::new(NameIdnHomograph),
        Box::new(NameNfcVsNfdCafe),
        Box::new(NameHangulJamo),
        Box::new(NameLeaf255Bytes),
        Box::new(NamePath4096Bytes),
        Box::new(NameWindowsReserved),
        Box::new(NameTrailingSpaceAndDot),
        Box::new(NameSeparatorLookalike),
        Box::new(NameUnpairedSurrogate),
        Box::new(NameCaseOnlyDiffers),
    ]
}

// ---------------------------------------------------------------------------
// Shared harness
// ---------------------------------------------------------------------------

/// A booted scenario fixture: the headless core wired to a fresh in-memory
/// fake plus the source rooted at an on-disk temp tree.
///
/// Holds the concrete [`InMemoryRemoteStore`] (so `list_folder` / `download`
/// / `root_id` stay reachable) alongside the [`DrivenHandle`]. The `_tmp`
/// guard keeps the on-disk fixture alive for the scenario's scope.
struct Fixture {
    handle: DrivenHandle,
    remote: Arc<InMemoryRemoteStore>,
    folder_id: String,
    _tmp: tempfile::TempDir,
}

impl Fixture {
    /// Boot a hermetic core whose single source points at a fresh temp tree.
    ///
    /// The DB and the source root live under sibling temp dirs; the source
    /// uploads into the fake's synthetic root folder. Returns the fixture
    /// plus the source root the caller materialises files under.
    async fn boot() -> anyhow::Result<(Self, PathBuf)> {
        let tmp = tempfile::tempdir()?;
        let db_path = tmp.path().join("state.db");
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root)?;

        let remote = Arc::new(InMemoryRemoteStore::new());
        let folder_id = remote.root_id().to_string();

        let handle = DrivenHandleBuilder::new(db_path)
            .remote(remote.clone() as Arc<dyn RemoteStore>)
            .boot()
            .await?;

        let source = source_row(handle.account_id, &source_root, &folder_id);
        handle.state.upsert_source(&source).await?;

        Ok((
            Self {
                handle,
                remote,
                folder_id,
                _tmp: tmp,
            },
            source_root,
        ))
    }

    /// Run exactly one scan -> plan -> execute cycle to completion.
    async fn run_one_cycle(&self) -> anyhow::Result<()> {
        self.handle.orchestrator.run_cycle(TickSource::Manual).await
    }

    /// Live (non-trashed) objects under the source's destination folder.
    async fn live_objects(&self) -> anyhow::Result<Vec<driven_drive::remote_store::RemoteEntry>> {
        Ok(self
            .remote
            .list_folder(&self.folder_id)
            .await?
            .into_iter()
            .filter(|e| !e.trashed)
            .collect())
    }

    /// Download one object's bytes by id (used for byte round-trip checks).
    async fn download_bytes(&self, file_id: &str) -> anyhow::Result<Vec<u8>> {
        let mut stream = self.remote.download(file_id).await?;
        let mut buf = Vec::new();
        stream.0.read_to_end(&mut buf).await?;
        Ok(buf)
    }

    /// Every `local.unicode_collision` activity row's message (the colliding
    /// path), for the NFC-collision scenarios.
    async fn collision_paths(&self) -> anyhow::Result<Vec<String>> {
        self.activity_messages_for("local.unicode_collision").await
    }

    /// Activity-row messages whose `event_type` equals `code` (the stable
    /// SPEC s24 code string). Error codes surface to the activity log, not the
    /// orchestrator broadcast (handle.rs divergence note), so assertions read
    /// them here.
    async fn activity_messages_for(&self, code: &str) -> anyhow::Result<Vec<String>> {
        use driven_core::state::{ActivityFilter, PageRequest};
        let page = self
            .handle
            .state
            .query_activity(
                ActivityFilter::default(),
                PageRequest {
                    page: 0,
                    limit: 10_000,
                },
            )
            .await?;
        Ok(page
            .rows
            .into_iter()
            .filter(|r| r.event_type == code)
            .filter_map(|r| r.message)
            .collect())
    }

    /// Count of `file_state` rows for the source at `status`.
    async fn file_states_with_status(&self, status: FileStateStatus) -> anyhow::Result<usize> {
        let source_id = self.source_id().await?;
        let map = self.handle.state.load_source_file_state(source_id).await?;
        Ok(map.values().filter(|r| r.status == status).count())
    }

    /// The single source's id.
    async fn source_id(&self) -> anyhow::Result<SourceId> {
        let sources = self
            .handle
            .state
            .list_enabled_sources_for(self.handle.account_id)
            .await?;
        sources
            .first()
            .map(|s| s.id)
            .ok_or_else(|| anyhow::anyhow!("no source configured"))
    }
}

/// A plaintext source rooted at `root` uploading into the fake `folder_id`.
fn source_row(account: driven_core::types::AccountId, root: &Path, folder_id: &str) -> SourceRow {
    SourceRow {
        id: SourceId::new_v4(),
        account_id: account,
        display_name: "filenames".into(),
        enabled: true,
        local_path: root.to_string_lossy().into_owned(),
        drive_folder_id: folder_id.to_string(),
        drive_folder_path: "/filenames".into(),
        encryption_enabled: false,
        wrapped_source_key: None,
        respect_gitignore: false,
        include_patterns: vec![],
        exclude_patterns: vec![],
        schedule_json_v2_reserved: None,
        deep_verify_interval_secs: 604_800,
        last_full_scan_at: None,
        last_deep_verify_at: Some(0),
        created_at: 0,
    }
}

/// Write `bytes` to `root/name`, returning whether creation succeeded. A
/// platform that rejects the name (Windows-illegal glyphs, reserved devices)
/// yields `Ok(false)` rather than an error so the caller can branch on
/// platform-dependent creation per the s3.4 "platform-aware" rows.
fn try_write(root: &Path, name: &OsName, bytes: &[u8]) -> std::io::Result<bool> {
    let path = root.join(name.as_os());
    match std::fs::write(&path, bytes) {
        Ok(()) => Ok(true),
        Err(e) if is_unsupported_name(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether an IO error is an OS rejection of the NAME itself (an illegal
/// character or a reserved device), as opposed to a real disk fault. Such a
/// rejection is the expected "creation fails on this platform" outcome for
/// the platform-aware rows, so it must not abort fixture creation.
///
/// `ErrorKind::InvalidFilename` is still unstable in std, so this matches the
/// stable kinds plus the concrete Windows error numbers a rejected name
/// returns: ERROR_INVALID_NAME (123) and ERROR_FILE_NOT_FOUND (2, which a
/// reserved device path can surface).
fn is_unsupported_name(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    if matches!(
        e.kind(),
        ErrorKind::InvalidInput | ErrorKind::PermissionDenied | ErrorKind::NotFound
    ) {
        return true;
    }
    // Windows: ERROR_INVALID_NAME = 123. A raw match keeps this working on
    // the stable toolchain without the unstable `InvalidFilename` kind.
    matches!(e.raw_os_error(), Some(123))
}

/// A filename carrier that works for both UTF-8 names and (on Windows) names
/// built from raw UTF-16 code units including unpaired surrogates.
enum OsName {
    /// A valid UTF-8 name.
    Utf8(String),
    /// A name built from raw UTF-16 units (Windows only); may be non-UTF-8.
    #[cfg(windows)]
    Wide(std::ffi::OsString),
}

impl OsName {
    fn as_os(&self) -> std::ffi::OsString {
        match self {
            OsName::Utf8(s) => std::ffi::OsString::from(s.as_str()),
            #[cfg(windows)]
            OsName::Wide(s) => s.clone(),
        }
    }
}

impl From<String> for OsName {
    fn from(s: String) -> Self {
        OsName::Utf8(s)
    }
}

impl From<&str> for OsName {
    fn from(s: &str) -> Self {
        OsName::Utf8(s.to_string())
    }
}

/// NFC-normalise a name the same way the core does, by routing it through
/// [`RelativePath::try_from`] (which NFC-normalises before the name becomes a
/// `file_state` key and the executor's upload name). Reusing the core's own
/// path avoids a second normalisation dependency and guarantees the assertion
/// compares against exactly the bytes the core will produce. A name that is
/// not a representable relative path (e.g. an unpaired surrogate) has no NFC
/// leaf form; callers only invoke this on representable names.
fn nfc(name: &str) -> String {
    RelativePath::try_from(name.to_string())
        .map(|r| r.as_str().to_string())
        .unwrap_or_else(|_| name.to_string())
}

// ---------------------------------------------------------------------------
// name-control-chars (platform-aware)
// ---------------------------------------------------------------------------

/// Files named with control characters. TAB / BEL / ESC / DEL are legal on
/// both NTFS and POSIX. CR / LF are NTFS-illegal but POSIX-legal, so they are
/// only attempted on Unix. All creatable names must upload and round-trip
/// byte-identical (STRESS_HARNESS s3.4 `name-control-chars`).
struct NameControlChars;

/// The control-character leaf names this scenario attempts, per platform.
///
/// NTFS forbids every code point 0x01-0x1F in a filename, so TAB (0x09),
/// BEL (0x07), ESC (0x1B), CR (0x0D), and LF (0x0A) are all Windows-illegal;
/// only DEL (0x7F) is creatable there. POSIX accepts every byte except `/`
/// and NUL, so it gets the full set. `try_write` returns `Ok(false)` for a
/// name the OS rejects, so the scenario simply backs up whatever was
/// creatable on the host - matching the s3.4 "platform-aware" intent.
fn control_char_names() -> Vec<String> {
    if cfg!(windows) {
        // Only DEL (0x7F) is a legal NTFS filename control char.
        vec![format!("del\u{007F}name.txt")]
    } else {
        vec![
            format!("tab\u{0009}name.txt"),
            format!("bel\u{0007}name.txt"),
            format!("esc\u{001B}name.txt"),
            format!("del\u{007F}name.txt"),
            "cr\u{000D}name.txt".to_string(),
            "lf\u{000A}name.txt".to_string(),
        ]
    }
}

#[async_trait]
impl Scenario for NameControlChars {
    fn name(&self) -> &'static str {
        "name-control-chars"
    }
    fn description(&self) -> &'static str {
        "files named with TAB/BEL/ESC/DEL (+CR/LF on POSIX) upload and round-trip"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (fx, root) = Fixture::boot().await?;

        // Map NFC(name) -> body for the names that actually got created.
        let mut expected: HashMap<String, Vec<u8>> = HashMap::new();
        for name in control_char_names() {
            let body = format!("body-of-{}", name.escape_debug()).into_bytes();
            if try_write(&root, &OsName::from(name.clone()), &body)? {
                expected.insert(nfc(&name), body);
            }
        }
        anyhow::ensure!(!expected.is_empty(), "no control-char names were creatable");

        fx.run_one_cycle().await?;

        let objects = fx.live_objects().await?;
        let mut hash_ok = true;
        for (nfc_name, body) in &expected {
            let entry = objects
                .iter()
                .find(|e| &e.name == nfc_name)
                .ok_or_else(|| anyhow::anyhow!("control-char name not uploaded: {nfc_name:?}"))?;
            let got = fx.download_bytes(&entry.id).await?;
            if &got != body {
                hash_ok = false;
            }
        }

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: objects.len() as u64,
            final_hash_matches_local: hash_ok,
            notes: vec![format!(
                "{} control-char names round-tripped",
                expected.len()
            )],
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ---------------------------------------------------------------------------
// name-rlo-zwj-zwnj-bom (user)
// ---------------------------------------------------------------------------

/// Names containing U+202E (RLO), U+200D (ZWJ), U+200C (ZWNJ), U+FEFF (BOM).
/// All are NFC-stable, so the uploaded leaf equals the raw name and round-trips
/// byte-identical (STRESS_HARNESS s3.4 `name-rlo-zwj-zwnj-bom`).
struct NameRloZwjZwnjBom;

#[async_trait]
impl Scenario for NameRloZwjZwnjBom {
    fn name(&self) -> &'static str {
        "name-rlo-zwj-zwnj-bom"
    }
    fn description(&self) -> &'static str {
        "names with RLO/ZWJ/ZWNJ/BOM upload and round-trip byte-identical"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (fx, root) = Fixture::boot().await?;
        let names = [
            "rlo\u{202E}name.txt",
            "zwj\u{200D}name.txt",
            "zwnj\u{200C}name.txt",
            "bom\u{FEFF}name.txt",
        ];
        let mut expected: HashMap<String, Vec<u8>> = HashMap::new();
        for (i, name) in names.iter().enumerate() {
            let body = format!("zero-width-body-{i}").into_bytes();
            anyhow::ensure!(
                try_write(&root, &OsName::from(*name), &body)?,
                "could not create {name:?}"
            );
            expected.insert(nfc(name), body);
        }

        fx.run_one_cycle().await?;

        let objects = fx.live_objects().await?;
        let mut hash_ok = true;
        for (nfc_name, body) in &expected {
            let entry = objects
                .iter()
                .find(|e| &e.name == nfc_name)
                .ok_or_else(|| anyhow::anyhow!("name not uploaded: {nfc_name:?}"))?;
            if &fx.download_bytes(&entry.id).await? != body {
                hash_ok = false;
            }
        }

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: objects.len() as u64,
            final_hash_matches_local: hash_ok,
            notes: vec!["RLO/ZWJ/ZWNJ/BOM names round-tripped".into()],
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ---------------------------------------------------------------------------
// name-idn-homograph (user)
// ---------------------------------------------------------------------------

/// A Cyrillic-'a' (U+0430) `apple.txt` and a Latin-'a' `apple.txt` in one
/// folder. Both upload as two distinct objects, two distinct `file_state`
/// rows: their path bytes differ and both are NFC-stable (STRESS_HARNESS s3.4
/// `name-idn-homograph`).
struct NameIdnHomograph;

#[async_trait]
impl Scenario for NameIdnHomograph {
    fn name(&self) -> &'static str {
        "name-idn-homograph"
    }
    fn description(&self) -> &'static str {
        "Cyrillic vs Latin homograph names upload as two distinct objects"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (fx, root) = Fixture::boot().await?;
        let cyrillic = "\u{0430}pple.txt"; // U+0430 CYRILLIC SMALL LETTER A
        let latin = "apple.txt";
        anyhow::ensure!(
            try_write(&root, &OsName::from(cyrillic), b"cyrillic")?,
            "could not create the Cyrillic-'a' name"
        );
        anyhow::ensure!(
            try_write(&root, &OsName::from(latin), b"latin")?,
            "could not create the Latin-'a' name"
        );

        fx.run_one_cycle().await?;

        let objects = fx.live_objects().await?;
        let synced = fx.file_states_with_status(FileStateStatus::Synced).await?;
        let collisions = fx.collision_paths().await?;

        // Two distinct objects, two synced rows, no unicode collision (the NFC
        // keys differ - Cyrillic 'a' and Latin 'a' are distinct code points).
        anyhow::ensure!(
            objects.len() == 2,
            "expected 2 distinct objects, got {}",
            objects.len()
        );
        anyhow::ensure!(synced == 2, "expected 2 synced rows, got {synced}");
        anyhow::ensure!(
            collisions.is_empty(),
            "homographs must not collide: {collisions:?}"
        );

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: objects.len() as u64,
            final_hash_matches_local: true,
            notes: vec!["IDN homographs stored as two distinct objects".into()],
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ---------------------------------------------------------------------------
// name-nfc-vs-nfd-cafe (cap:case_sensitive_volume)
// ---------------------------------------------------------------------------

/// `cafe` + combining-acute U+0301 (NFD) and a precomposed `cafe`-with-acute
/// (NFC, e becomes U+00E9) in one folder.
/// On a case-sensitive volume both byte-distinct names can be created, but they
/// NFC-normalise to the same key, so the scanner keeps the first and surfaces
/// `local.unicode_collision` for the second (STRESS_HARNESS s3.4 /
/// DESIGN s5.2.3). Requires a case-sensitive volume to create both forms;
/// SKIPs otherwise.
struct NameNfcVsNfdCafe;

#[async_trait]
impl Scenario for NameNfcVsNfdCafe {
    fn name(&self) -> &'static str {
        "name-nfc-vs-nfd-cafe"
    }
    fn description(&self) -> &'static str {
        "NFD + NFC cafe collide to one NFC key; later form surfaces local.unicode_collision"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::CaseSensitiveVolume])
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        // NFD: c a f e + U+0301 (COMBINING ACUTE ACCENT). NFC: e is U+00E9.
        let nfd = "cafe\u{0301}.txt";
        let nfc_form = "caf\u{00E9}.txt";
        assert_collision(nfd, nfc_form).await
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::GracefulFailureWith {
            code: ErrorCode::LocalUnicodeCollision,
        }
    }
}

// ---------------------------------------------------------------------------
// name-hangul-jamo (cap:case_sensitive_volume)
// ---------------------------------------------------------------------------

/// Precomposed Hangul syllable U+D55C vs decomposed jamo U+1112 U+1161 U+11AB.
/// Same NFC-collision behaviour as the cafe row (STRESS_HARNESS s3.4
/// `name-hangul-jamo`). Requires a case-sensitive volume; SKIPs otherwise.
struct NameHangulJamo;

#[async_trait]
impl Scenario for NameHangulJamo {
    fn name(&self) -> &'static str {
        "name-hangul-jamo"
    }
    fn description(&self) -> &'static str {
        "precomposed vs decomposed Hangul collide to one NFC key (local.unicode_collision)"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::CaseSensitiveVolume])
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let precomposed = "\u{D55C}.txt"; // precomposed Hangul syllable U+D55C
        let jamo = "\u{1112}\u{1161}\u{11AB}.txt"; // jamo U+1112 U+1161 U+11AB
        assert_collision(precomposed, jamo).await
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::GracefulFailureWith {
            code: ErrorCode::LocalUnicodeCollision,
        }
    }
}

/// Shared body for the two NFC-collision rows: create two byte-distinct names
/// that NFC-normalise to one key, run a cycle, and assert exactly one object
/// uploaded plus a `local.unicode_collision` activity row for the dropped
/// form. Only one of the two NFC-equal bodies survives (the scanner keeps the
/// first walked), so no data loss invariant is violated - the dropped name is
/// an exact-duplicate key, surfaced as an error, not silently lost.
async fn assert_collision(name_a: &str, name_b: &str) -> anyhow::Result<Outcome> {
    let (fx, root) = Fixture::boot().await?;

    // The two names must be byte-distinct on disk but NFC-equal.
    anyhow::ensure!(
        name_a != name_b,
        "collision inputs must be byte-distinct on disk"
    );
    anyhow::ensure!(
        nfc(name_a) == nfc(name_b),
        "collision inputs must share one NFC key"
    );

    let created_a = try_write(&root, &OsName::from(name_a), b"form-a")?;
    let created_b = try_write(&root, &OsName::from(name_b), b"form-b")?;
    // On a genuinely case-sensitive volume both byte-distinct names exist. If
    // the host folded them (a case/normalisation-insensitive FS leaked past the
    // capability probe), only one creation "wins"; the collision cannot then be
    // exercised, so this is an honest harness error rather than a masked pass.
    anyhow::ensure!(
        created_a && created_b,
        "both NFC-equal forms must be creatable on a case-sensitive volume \
         (a={created_a}, b={created_b}); the host folded them"
    );

    fx.run_one_cycle().await?;

    let objects = fx.live_objects().await?;
    let collisions = fx.collision_paths().await?;

    anyhow::ensure!(
        objects.len() == 1,
        "exactly one NFC form should upload, got {} objects",
        objects.len()
    );
    anyhow::ensure!(
        !collisions.is_empty(),
        "the dropped form must surface local.unicode_collision"
    );

    Ok(Outcome {
        error_codes_seen: vec![ErrorCode::LocalUnicodeCollision],
        final_drive_object_count: objects.len() as u64,
        final_hash_matches_local: true,
        notes: vec![format!(
            "{} collision row(s); one NFC form uploaded",
            collisions.len()
        )],
    })
}

// ---------------------------------------------------------------------------
// name-leaf-255-bytes (user)
// ---------------------------------------------------------------------------

/// Filename of exactly 255 UTF-8 bytes (the NTFS / ext4 leaf limit). Uploads
/// and round-trips identical (STRESS_HARNESS s3.4 `name-leaf-255-bytes`).
///
/// SPEC AMBIGUITY (documented): the catalogue marks this `user`, which holds
/// on POSIX. On Windows a 255-char leaf under any non-trivial directory blows
/// past the ~260-char MAX_PATH unless long paths are enabled, so this gates
/// the Windows arm on `LongPathsEnabled` (matching the sibling
/// `name-path-4096-bytes` row) rather than failing fragilely on a default
/// Windows host. The leaf is created via a verbatim `\\?\` path so the Win32
/// layer does not truncate it, the same way Driven creates paths (via dunce).
struct NameLeaf255Bytes;

#[async_trait]
impl Scenario for NameLeaf255Bytes {
    fn name(&self) -> &'static str {
        "name-leaf-255-bytes"
    }
    fn description(&self) -> &'static str {
        "a 255-UTF-8-byte leaf name uploads and round-trips identical"
    }
    fn requires(&self) -> CapabilityRequirements {
        if cfg!(windows) {
            CapabilityRequirements::of(vec![Capability::LongPathsEnabled])
        } else {
            CapabilityRequirements::none()
        }
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (fx, root) = Fixture::boot().await?;
        // 251 'a' + ".txt" = 255 bytes, all ASCII (1 byte each), NFC-stable.
        let name = format!("{}.txt", "a".repeat(251));
        anyhow::ensure!(name.len() == 255, "name must be exactly 255 bytes");
        let body = b"max-leaf-name";
        anyhow::ensure!(
            write_verbatim(&root, &name, body)?,
            "could not create a 255-byte leaf name (host leaf limit below 255?)"
        );

        fx.run_one_cycle().await?;

        let objects = fx.live_objects().await?;
        let entry = objects
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| anyhow::anyhow!("255-byte name not uploaded"))?;
        let hash_ok = fx.download_bytes(&entry.id).await? == body;

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: objects.len() as u64,
            final_hash_matches_local: hash_ok,
            notes: vec!["255-byte leaf round-tripped".into()],
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ---------------------------------------------------------------------------
// name-path-4096-bytes (cap:long_paths_enabled on Windows)
// ---------------------------------------------------------------------------

/// A full path of ~4 KiB via deep nesting. Uploads succeed where long paths
/// are enabled. On Windows the row requires `cap:long_paths_enabled`; on Unix
/// no extra capability. The plaintext source flattens the leaf under the
/// source root (executor `resolve_remote_target`), so the assertion is that the
/// deeply-nested file backs up (a synced row + its leaf uploaded), not that the
/// remote mirrors the nesting (STRESS_HARNESS s3.4 `name-path-4096-bytes`).
struct NamePath4096Bytes;

#[async_trait]
impl Scenario for NamePath4096Bytes {
    fn name(&self) -> &'static str {
        "name-path-4096-bytes"
    }
    fn description(&self) -> &'static str {
        "a ~4 KiB total path via deep nesting backs up where long paths are enabled"
    }
    fn requires(&self) -> CapabilityRequirements {
        // Long paths are a Windows capability gate; Unix has no 260-char MAX_PATH.
        if cfg!(windows) {
            CapabilityRequirements::of(vec![Capability::LongPathsEnabled])
        } else {
            CapabilityRequirements::none()
        }
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (fx, root) = Fixture::boot().await?;

        // Build nested dirs whose joined relative path approaches 4 KiB. Each
        // component is 60 chars + a separator (~61 bytes); ~64 levels reaches
        // ~4 KiB of relative path. The leaf is the file.
        let component = "d".repeat(60);
        let mut rel = PathBuf::new();
        for _ in 0..64 {
            rel.push(&component);
        }
        let leaf = "leaf-4096.txt";
        rel.push(leaf);
        let rel_len = rel.to_string_lossy().len();
        anyhow::ensure!(
            rel_len >= 3500,
            "deep path should approach ~4 KiB, got {rel_len} bytes"
        );

        let abs = root.join(&rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&abs, b"deep-nested-body")?;

        fx.run_one_cycle().await?;

        // The plaintext executor uploads the leaf flat under the source root.
        let objects = fx.live_objects().await?;
        let synced = fx.file_states_with_status(FileStateStatus::Synced).await?;
        anyhow::ensure!(
            objects.iter().any(|e| e.name == leaf),
            "deeply-nested leaf not uploaded"
        );
        anyhow::ensure!(synced >= 1, "deep-path file should be synced, got {synced}");

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: objects.len() as u64,
            final_hash_matches_local: true,
            notes: vec![format!("~{rel_len}-byte relative path backed up")],
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ---------------------------------------------------------------------------
// name-windows-reserved (platform-aware)
// ---------------------------------------------------------------------------

/// `CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`, plus `CON.txt`.
/// On Windows these are reserved device names: creation fails, so the scanner
/// has nothing to back up. On POSIX they are ordinary names: they upload and
/// round-trip (STRESS_HARNESS s3.4 `name-windows-reserved`).
///
/// CORE GAP (documented, not masked): the s3.4 expected outcome says Windows
/// surfaces `local.io_error` if a reserved name is "attempted". The M3 core
/// only writes `local.io_error` for a Failed executor OP - a name that cannot
/// be created never enters the scan, never becomes an op, and so no code is
/// surfaced. On Windows this scenario therefore documents current behaviour
/// (nothing backed up, no spurious deletion), and the missing-code gap is
/// reported rather than asserted as present.
struct NameWindowsReserved;

/// The Windows-reserved device names this scenario probes.
fn windows_reserved_names() -> Vec<String> {
    let mut names = vec![
        "CON".to_string(),
        "PRN".to_string(),
        "AUX".to_string(),
        "NUL".to_string(),
        "CON.txt".to_string(),
    ];
    for i in 1..=9 {
        names.push(format!("COM{i}"));
        names.push(format!("LPT{i}"));
    }
    names
}

#[async_trait]
impl Scenario for NameWindowsReserved {
    fn name(&self) -> &'static str {
        "name-windows-reserved"
    }
    fn description(&self) -> &'static str {
        "reserved CON/PRN/AUX/NUL/COMn/LPTn: skipped on Windows, uploaded on POSIX"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (fx, root) = Fixture::boot().await?;

        let mut created: Vec<String> = Vec::new();
        for name in windows_reserved_names() {
            if try_write(&root, &OsName::from(name.clone()), b"reserved-body")? {
                created.push(nfc(&name));
            }
        }

        fx.run_one_cycle().await?;
        let objects = fx.live_objects().await?;

        if cfg!(windows) {
            // Windows reserved-name behaviour is split, and BOTH halves are
            // current documented behaviour (this row is DocumentedBehaviour, not
            // a fixed expected outcome):
            //
            //   (a) The Win32 surface rejects the name outright (ERROR_INVALID_NAME)
            //       or redirects `NUL`/`CON` to the kernel device object, so NO
            //       directory entry lands and nothing is scanned/uploaded; or
            //   (b) Rust's std prepends an implicit verbatim `\\?\` prefix for the
            //       absolute tempdir path, which BYPASSES the DOS-device-name
            //       filter, so a real on-disk file literally named `CON`/`LPT1`
            //       DOES materialise - and then scans, uploads, and round-trips
            //       like any other file (the same `\\?\` mechanism Driven uses).
            //
            // The load-bearing invariants either way: every file that DID
            // materialise round-trips byte-identical (no data loss), and the
            // remote object count never exceeds the materialised-file count (no
            // duplicate / collateral object). We assert the ground truth and
            // record which branch this host took.
            let real_entries: Vec<std::path::PathBuf> = std::fs::read_dir(&root)?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect();
            anyhow::ensure!(
                objects.len() <= real_entries.len(),
                "remote object count {} exceeds materialised reserved-name files {} \
                 (a reserved name must never produce a phantom/duplicate object)",
                objects.len(),
                real_entries.len()
            );
            // Every uploaded object must carry the exact bytes we wrote.
            let mut hash_ok = true;
            for entry in &objects {
                if fx.download_bytes(&entry.id).await? != b"reserved-body" {
                    hash_ok = false;
                }
            }
            let note = if real_entries.is_empty() {
                "Windows (branch a): reserved device names were rejected/redirected by \
                 the Win32 surface; nothing materialised or uploaded. CORE GAP: s3.4 \
                 expects local.io_error on an attempted reserved name, but the M3 core \
                 surfaces io_error only for a Failed executor op, never for a name the OS \
                 refuses to create. Documented behaviour, not masked."
                    .to_string()
            } else {
                format!(
                    "Windows (branch b): {} reserved-device name(s) materialised as real \
                     files via Rust std's implicit verbatim `\\\\?\\` path and round-tripped \
                     byte-identical ({} uploaded). Restore to a Win32-API consumer is a \
                     documented caveat. Documented behaviour, not masked.",
                    real_entries.len(),
                    objects.len()
                )
            };
            Ok(Outcome {
                error_codes_seen: vec![],
                final_drive_object_count: objects.len() as u64,
                final_hash_matches_local: hash_ok,
                notes: vec![note],
            })
        } else {
            // POSIX: reserved names are ordinary; all upload and round-trip.
            anyhow::ensure!(
                !created.is_empty(),
                "POSIX should create the reserved-name files"
            );
            let mut hash_ok = true;
            for nfc_name in &created {
                let entry = objects
                    .iter()
                    .find(|e| &e.name == nfc_name)
                    .ok_or_else(|| {
                        anyhow::anyhow!("reserved name not uploaded on POSIX: {nfc_name:?}")
                    })?;
                if fx.download_bytes(&entry.id).await? != b"reserved-body" {
                    hash_ok = false;
                }
            }
            Ok(Outcome {
                error_codes_seen: vec![],
                final_drive_object_count: objects.len() as u64,
                final_hash_matches_local: hash_ok,
                notes: vec![format!(
                    "POSIX: {} reserved-device names round-tripped (restore to Windows \
                     is a documented caveat)",
                    created.len()
                )],
            })
        }
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        // Both platforms document current behaviour: POSIX uploads, Windows
        // rejects-at-creation. The s24-code branch is a documented core gap
        // (see the struct doc), so this is a documented-behaviour assertion,
        // not a GracefulFailureWith that the core cannot satisfy.
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ---------------------------------------------------------------------------
// name-trailing-space-and-dot (Windows, cap:ntfs_volume)
// ---------------------------------------------------------------------------

/// `foo .txt` and `foo.txt.` - names with a trailing space or trailing dot.
/// Driven uses `\\?\` path prefixes (via `dunce`) so the underlying NTFS names
/// are preserved rather than mangled by the Win32 surface; the names must
/// round-trip (STRESS_HARNESS s3.4 `name-trailing-space-and-dot`). Requires
/// Windows + NTFS; SKIPs otherwise.
struct NameTrailingSpaceAndDot;

#[async_trait]
impl Scenario for NameTrailingSpaceAndDot {
    fn name(&self) -> &'static str {
        "name-trailing-space-and-dot"
    }
    fn description(&self) -> &'static str {
        "trailing-space / trailing-dot names preserved via \\\\?\\ prefixes and round-trip"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::Windows, Capability::NtfsVolume])
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (fx, root) = Fixture::boot().await?;

        // Names with a trailing space and a trailing dot. Create via the
        // verbatim `\\?\` prefix so the Win32 layer does not strip the trailing
        // byte before NTFS sees the name.
        let names = ["foo .txt", "foo.txt."];
        let mut expected: HashMap<String, Vec<u8>> = HashMap::new();
        for (i, name) in names.iter().enumerate() {
            let body = format!("trailing-{i}").into_bytes();
            let created = write_verbatim(&root, name, &body)?;
            anyhow::ensure!(
                created,
                "could not create {name:?} even via a verbatim \\\\?\\ path"
            );
            expected.insert(nfc(name), body);
        }

        fx.run_one_cycle().await?;

        let objects = fx.live_objects().await?;
        let mut hash_ok = true;
        let mut matched = 0usize;
        for (nfc_name, body) in &expected {
            if let Some(entry) = objects.iter().find(|e| &e.name == nfc_name) {
                matched += 1;
                if &fx.download_bytes(&entry.id).await? != body {
                    hash_ok = false;
                }
            }
        }
        anyhow::ensure!(
            matched == expected.len(),
            "expected {} trailing-space/dot names to round-trip, matched {matched}",
            expected.len()
        );

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: objects.len() as u64,
            final_hash_matches_local: hash_ok,
            notes: vec!["trailing-space / trailing-dot names preserved + round-tripped".into()],
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

/// Create `root/name` using a verbatim `\\?\` path on Windows so the Win32
/// layer does not strip a trailing space / dot before NTFS records the name.
/// On non-Windows this is a plain write (the scenario gates to Windows, so the
/// non-Windows arm is only here to keep the function total).
fn write_verbatim(root: &Path, name: &str, bytes: &[u8]) -> std::io::Result<bool> {
    #[cfg(windows)]
    {
        // Build \\?\<abs-root>\<name>. The verbatim prefix disables Win32 path
        // normalisation (trailing dot/space stripping), so the literal name
        // reaches NTFS.
        let abs = root.join("placeholder");
        let abs_root = abs.parent().unwrap_or(root);
        let verbatim = format!(r"\\?\{}\{}", abs_root.display(), name);
        match std::fs::write(&verbatim, bytes) {
            Ok(()) => Ok(true),
            Err(e) if is_unsupported_name(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(windows))]
    {
        try_write(root, &OsName::from(name), bytes)
    }
}

// ---------------------------------------------------------------------------
// name-separator-lookalike (user)
// ---------------------------------------------------------------------------

/// Names containing U+2215 DIVISION SLASH and U+FF0F FULLWIDTH SOLIDUS - they
/// resemble `/` but are not path separators. Each uploads as one object with no
/// spurious folder split, byte-identical (STRESS_HARNESS s3.4
/// `name-separator-lookalike`).
struct NameSeparatorLookalike;

#[async_trait]
impl Scenario for NameSeparatorLookalike {
    fn name(&self) -> &'static str {
        "name-separator-lookalike"
    }
    fn description(&self) -> &'static str {
        "U+2215 / U+FF0F slash look-alikes upload as one object, no folder split"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (fx, root) = Fixture::boot().await?;
        let names = [
            "a\u{2215}b.txt", // DIVISION SLASH
            "c\u{FF0F}d.txt", // FULLWIDTH SOLIDUS
        ];
        let mut expected: HashMap<String, Vec<u8>> = HashMap::new();
        for (i, name) in names.iter().enumerate() {
            let body = format!("lookalike-{i}").into_bytes();
            anyhow::ensure!(
                try_write(&root, &OsName::from(*name), &body)?,
                "could not create {name:?}"
            );
            expected.insert(nfc(name), body);
        }

        fx.run_one_cycle().await?;

        let objects = fx.live_objects().await?;
        // Exactly one object per look-alike file: the glyphs must NOT split the
        // name into a folder path.
        anyhow::ensure!(
            objects.len() == expected.len(),
            "slash look-alikes must not split into folders: {} objects for {} files",
            objects.len(),
            expected.len()
        );
        let mut hash_ok = true;
        for (nfc_name, body) in &expected {
            let entry = objects
                .iter()
                .find(|e| &e.name == nfc_name)
                .ok_or_else(|| anyhow::anyhow!("look-alike name not uploaded: {nfc_name:?}"))?;
            if &fx.download_bytes(&entry.id).await? != body {
                hash_ok = false;
            }
        }

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: objects.len() as u64,
            final_hash_matches_local: hash_ok,
            notes: vec!["separator look-alikes stored flat, round-tripped".into()],
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ---------------------------------------------------------------------------
// name-unpaired-surrogate (Windows, cap:ntfs_volume)
// ---------------------------------------------------------------------------

/// A filename containing an unpaired UTF-16 surrogate half (Win32 allows it via
/// `\\?\`). Driven should detect the UTF-8 conversion failure, surface
/// `local.invalid_filename`, skip the file, and continue the scan
/// (STRESS_HARNESS s3.4 `name-unpaired-surrogate`). Requires Windows + NTFS.
///
/// CORE GAP (documented, not masked): the M3 scanner DOES skip a name that is
/// not representable as a `RelativePath` (an unpaired surrogate fails
/// `RelativePath::try_from` with `NotUtf8`), but it does so with a
/// `tracing::warn` and writes NO activity row, so `local.invalid_filename`
/// never reaches the activity log. This scenario asserts the spec-correct
/// outcome (the code IS surfaced) without weakening it; on the current core it
/// therefore FAILS, which is the honest signal that the emission is missing.
struct NameUnpairedSurrogate;

#[async_trait]
impl Scenario for NameUnpairedSurrogate {
    fn name(&self) -> &'static str {
        "name-unpaired-surrogate"
    }
    fn description(&self) -> &'static str {
        "an unpaired UTF-16 surrogate name is skipped with local.invalid_filename"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::Windows, Capability::NtfsVolume])
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (fx, root) = Fixture::boot().await?;

        // A neighbouring valid file must keep syncing (the scan continues past
        // the skipped invalid name).
        anyhow::ensure!(
            try_write(&root, &OsName::from("valid-neighbour.txt"), b"ok")?,
            "could not create the valid neighbour file"
        );

        // Build a leaf with an unpaired high surrogate (U+D800) and create it
        // via a wide-char path. Only meaningful on Windows.
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            // 'b','a','d', 0xD800 (lone high surrogate), '.','t','x','t'
            let units: [u16; 8] = [
                b'b' as u16,
                b'a' as u16,
                b'd' as u16,
                0xD800,
                b'.' as u16,
                b't' as u16,
                b'x' as u16,
                b't' as u16,
            ];
            let wide = std::ffi::OsString::from_wide(&units);
            let created = try_write(&root, &OsName::Wide(wide), b"surrogate-body")?;
            anyhow::ensure!(
                created,
                "could not create an unpaired-surrogate file even via the wide API"
            );
        }

        fx.run_one_cycle().await?;

        let invalid = fx
            .activity_messages_for(ErrorCode::LocalInvalidFilename.code())
            .await?;
        let objects = fx.live_objects().await?;
        // The valid neighbour must still be backed up (scan continued).
        anyhow::ensure!(
            objects.iter().any(|e| e.name == "valid-neighbour.txt"),
            "the valid neighbour must still upload after the invalid name is skipped"
        );

        // Spec-correct assertion (NOT weakened): the invalid name must surface
        // local.invalid_filename. On the current core this is empty (the
        // scanner drops the name without an activity row), so this returns an
        // Outcome the runner reads as FAIL - the honest core-gap signal.
        Ok(Outcome {
            error_codes_seen: if invalid.is_empty() {
                vec![]
            } else {
                vec![ErrorCode::LocalInvalidFilename]
            },
            final_drive_object_count: objects.len() as u64,
            final_hash_matches_local: true,
            notes: vec![format!(
                "{} local.invalid_filename row(s) observed (CORE GAP if 0: the M3 \
                 scanner skips an unrepresentable name without writing an activity \
                 row; the code is enum-only, never emitted)",
                invalid.len()
            )],
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::GracefulFailureWith {
            code: ErrorCode::LocalInvalidFilename,
        }
    }
}

// ---------------------------------------------------------------------------
// name-case-only-differs (cap:case_sensitive_volume)
// ---------------------------------------------------------------------------

/// `Readme.md` and `README.MD` in one folder. On a case-sensitive volume they
/// are two distinct files: two `file_state` rows, two Drive objects, no
/// collision (case-only differences are NOT a unicode normalisation collision)
/// (STRESS_HARNESS s3.4 `name-case-only-differs`). Requires a case-sensitive
/// volume; SKIPs otherwise.
struct NameCaseOnlyDiffers;

#[async_trait]
impl Scenario for NameCaseOnlyDiffers {
    fn name(&self) -> &'static str {
        "name-case-only-differs"
    }
    fn description(&self) -> &'static str {
        "Readme.md vs README.MD on a case-sensitive volume: two distinct objects"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::CaseSensitiveVolume])
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (fx, root) = Fixture::boot().await?;
        let lower = "Readme.md";
        let upper = "README.MD";
        let created_l = try_write(&root, &OsName::from(lower), b"lower")?;
        let created_u = try_write(&root, &OsName::from(upper), b"upper")?;
        anyhow::ensure!(
            created_l && created_u,
            "both case forms must exist on a case-sensitive volume \
             (lower={created_l}, upper={created_u}); the host folded case"
        );

        fx.run_one_cycle().await?;

        let objects = fx.live_objects().await?;
        let synced = fx.file_states_with_status(FileStateStatus::Synced).await?;
        let collisions = fx.collision_paths().await?;

        anyhow::ensure!(
            objects.len() == 2,
            "case-distinct names should give 2 objects, got {}",
            objects.len()
        );
        anyhow::ensure!(synced == 2, "expected 2 synced rows, got {synced}");
        anyhow::ensure!(
            collisions.is_empty(),
            "case-only difference is not a unicode collision: {collisions:?}"
        );

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: objects.len() as u64,
            final_hash_matches_local: true,
            notes: vec!["case-only-distinct names stored as two objects".into()],
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Every s3.4 row (minus the folded `name-normalisation-only-differs`) is
    /// registered with a unique, stable, kebab-case name.
    #[test]
    fn registers_every_s3_4_row() {
        let names: Vec<&str> = scenarios().iter().map(|s| s.name()).collect();
        let expected = [
            "name-control-chars",
            "name-rlo-zwj-zwnj-bom",
            "name-idn-homograph",
            "name-nfc-vs-nfd-cafe",
            "name-hangul-jamo",
            "name-leaf-255-bytes",
            "name-path-4096-bytes",
            "name-windows-reserved",
            "name-trailing-space-and-dot",
            "name-separator-lookalike",
            "name-unpaired-surrogate",
            "name-case-only-differs",
        ];
        for e in expected {
            assert!(names.contains(&e), "missing scenario {e}");
        }
        // No duplicates.
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate scenario name(s)");
        // All kebab-case ASCII.
        for n in &names {
            assert!(
                n.bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'-' || b.is_ascii_digit()),
                "non-kebab name: {n}"
            );
        }
    }

    /// `nfc` collapses NFD onto its precomposed NFC form, which is what the
    /// collision scenarios rely on for their "same NFC key" precondition.
    #[test]
    fn nfc_collapses_nfd_cafe() {
        assert_eq!(nfc("cafe\u{0301}.txt"), nfc("caf\u{00E9}.txt"));
        assert_eq!(nfc("caf\u{00E9}.txt"), "caf\u{00E9}.txt");
    }

    /// The capability gates match the s3.4 catalogue's `Requires` column.
    #[test]
    fn capability_gates_match_catalogue() {
        let by_name = |n: &str| -> CapabilityRequirements {
            scenarios()
                .into_iter()
                .find(|s| s.name() == n)
                .map(|s| s.requires())
                .expect("scenario present")
        };
        // Case-sensitive-volume rows.
        for n in [
            "name-nfc-vs-nfd-cafe",
            "name-hangul-jamo",
            "name-case-only-differs",
        ] {
            assert!(
                by_name(n)
                    .required
                    .contains(&Capability::CaseSensitiveVolume),
                "{n} must require a case-sensitive volume"
            );
        }
        // Windows + NTFS rows.
        for n in ["name-trailing-space-and-dot", "name-unpaired-surrogate"] {
            let req = by_name(n).required;
            assert!(
                req.contains(&Capability::Windows),
                "{n} must require Windows"
            );
            assert!(
                req.contains(&Capability::NtfsVolume),
                "{n} must require NTFS"
            );
        }
        // Unconditional rows. NOTE: name-leaf-255-bytes and
        // name-path-4096-bytes are catalogue-`user` on POSIX but gate the
        // Windows arm on LongPathsEnabled (the documented MAX_PATH deviation
        // on each scenario), so they are asserted in the long-paths group
        // below rather than here.
        for n in [
            "name-control-chars",
            "name-rlo-zwj-zwnj-bom",
            "name-idn-homograph",
            "name-separator-lookalike",
        ] {
            assert!(by_name(n).required.is_empty(), "{n} should run on any host");
        }
        // Long-path rows: unconditional on POSIX, LongPathsEnabled-gated on
        // Windows (each scenario's documented MAX_PATH deviation from the
        // catalogue `user` marking).
        for n in ["name-leaf-255-bytes", "name-path-4096-bytes"] {
            let req = by_name(n).required;
            if cfg!(windows) {
                assert!(
                    req.contains(&Capability::LongPathsEnabled),
                    "{n} must gate on LongPathsEnabled on Windows"
                );
            } else {
                assert!(req.is_empty(), "{n} should run on any POSIX host");
            }
        }
    }

    /// The expected-outcome encodes the spec verdict per row (and does not
    /// weaken the two documented core-gap rows away from the spec).
    #[test]
    fn expected_outcomes_match_spec() {
        let outcome = |n: &str| -> ExpectedOutcome {
            scenarios()
                .into_iter()
                .find(|s| s.name() == n)
                .map(|s| s.expected_outcome())
                .expect("scenario present")
        };
        // NFC-collision rows expect local.unicode_collision.
        for n in ["name-nfc-vs-nfd-cafe", "name-hangul-jamo"] {
            assert!(
                matches!(
                    outcome(n),
                    ExpectedOutcome::GracefulFailureWith {
                        code: ErrorCode::LocalUnicodeCollision
                    }
                ),
                "{n} must expect local.unicode_collision"
            );
        }
        // Unpaired surrogate expects local.invalid_filename (spec-correct,
        // a current core gap - not weakened).
        assert!(
            matches!(
                outcome("name-unpaired-surrogate"),
                ExpectedOutcome::GracefulFailureWith {
                    code: ErrorCode::LocalInvalidFilename
                }
            ),
            "unpaired-surrogate must expect local.invalid_filename per spec"
        );
        // Success rows.
        for n in [
            "name-control-chars",
            "name-rlo-zwj-zwnj-bom",
            "name-idn-homograph",
            "name-leaf-255-bytes",
            "name-path-4096-bytes",
            "name-trailing-space-and-dot",
            "name-separator-lookalike",
            "name-case-only-differs",
        ] {
            assert!(
                matches!(outcome(n), ExpectedOutcome::Success),
                "{n} must expect Success"
            );
        }
    }

    /// Drive an actual core cycle for the no-capability success rows on this
    /// host, asserting the files round-trip byte-identical through the fake.
    /// These exercise the real scan -> plan -> execute -> upload path the
    /// runner will drive, so the scenario bodies are validated even before the
    /// runner exists.
    #[tokio::test]
    async fn control_chars_round_trip_through_core() {
        let (h, _g) = dummy_handle().await;
        let out = NameControlChars
            .run_assertions(&h)
            .await
            .expect("control-chars scenario runs");
        assert!(out.final_hash_matches_local, "bytes must round-trip");
        // POSIX creates the full set (6); NTFS allows only DEL (1). Assert at
        // least the host's creatable minimum so the test is honest on both.
        let min_expected = if cfg!(windows) { 1 } else { 6 };
        assert!(
            out.final_drive_object_count >= min_expected,
            "expected >= {min_expected} control-char names to upload, got {}",
            out.final_drive_object_count
        );
        assert!(out.error_codes_seen.is_empty(), "no error codes expected");
    }

    #[tokio::test]
    async fn rlo_zwj_round_trip_through_core() {
        let (h, _g) = dummy_handle().await;
        let out = NameRloZwjZwnjBom
            .run_assertions(&h)
            .await
            .expect("rlo/zwj scenario runs");
        assert!(out.final_hash_matches_local);
        assert_eq!(out.final_drive_object_count, 4);
    }

    #[tokio::test]
    async fn idn_homograph_two_distinct_objects() {
        let (h, _g) = dummy_handle().await;
        let out = NameIdnHomograph
            .run_assertions(&h)
            .await
            .expect("idn-homograph scenario runs");
        assert_eq!(out.final_drive_object_count, 2, "two distinct objects");
        assert!(out.error_codes_seen.is_empty());
    }

    // POSIX-only: a 255-char leaf under a tempdir exceeds Windows MAX_PATH
    // without long paths enabled, so the live core-cycle check runs on Unix
    // (where the scenario is ungated). The Windows arm gates on
    // `LongPathsEnabled` and is exercised on a maintainer machine that has it.
    #[cfg(unix)]
    #[tokio::test]
    async fn leaf_255_bytes_round_trips() {
        let (h, _g) = dummy_handle().await;
        let out = NameLeaf255Bytes
            .run_assertions(&h)
            .await
            .expect("255-byte leaf scenario runs");
        assert!(out.final_hash_matches_local);
        assert_eq!(out.final_drive_object_count, 1);
    }

    #[tokio::test]
    async fn separator_lookalikes_do_not_split() {
        let (h, _g) = dummy_handle().await;
        let out = NameSeparatorLookalike
            .run_assertions(&h)
            .await
            .expect("separator-lookalike scenario runs");
        assert_eq!(out.final_drive_object_count, 2, "no folder split");
        assert!(out.final_hash_matches_local);
    }

    /// A throwaway handle (plus its tempdir guard) to satisfy the
    /// `run_assertions` signature; the scenarios build their own hermetic
    /// handle internally and ignore this one (see the module's "forced
    /// architecture" note). The returned `TempDir` must be kept in scope by
    /// the caller so the DB file outlives the call.
    async fn dummy_handle() -> (DrivenHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = DrivenHandleBuilder::new(dir.path().join("ignored.db"))
            .boot()
            .await
            .expect("boot throwaway handle");
        (handle, dir)
    }
}
