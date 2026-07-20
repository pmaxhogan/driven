//! Real-Drive contract suite (ROADMAP M4), gated on `DRIVEN_E2E_*` env vars.
//!
//! These run the SAME portable [`common`] scenarios as `fake_contract.rs`,
//! but against a live [`GoogleDriveStore`] built from the maintainer's own
//! refresh token (exercising the production OAuth refresh path; ROADMAP M4).
//! When `DRIVEN_E2E_REFRESH_TOKEN` + `DRIVEN_E2E_DEST_FOLDER_ID` +
//! `DRIVEN_OAUTH_CLIENT_SECRET` are absent the suite prints a clear
//! "skipping real-Drive e2e" line and returns Ok WITHOUT failing, so CI on a
//! machine with no creds stays green. The tests are NOT `#[ignore]`d - they
//! are wired and flip on the moment the gate is set (an honest env gate, not a
//! faked-green skip). See `design/E2E_REAL.md` for minting the token + the CI
//! `chaos-real-drive` job.
//!
//! Each test builds a live store from the refresh token, operates inside a
//! fresh UUID-named child folder under the dest folder, and trashes that child
//! folder on success AND on failure (ROADMAP M4).

use driven_drive::google::token_store::RefreshingTokenSource;
use driven_drive::google::GoogleDriveStore;
use driven_drive::remote_store::{DriveContext, RemoteStore};

mod common;

/// Env var carrying the maintainer's Google refresh token (ROADMAP M4).
const ENV_REFRESH_TOKEN: &str = "DRIVEN_E2E_REFRESH_TOKEN";
/// Env var carrying the destination Drive folder id the tests upload under
/// (ROADMAP M4; each test uses a UUID-named child of it).
const ENV_DEST_FOLDER_ID: &str = "DRIVEN_E2E_DEST_FOLDER_ID";
/// Env var carrying a Google Shared Drive id (issue #7). When set (alongside
/// the refresh-token gate) the Shared Drive contract tests run: each creates a
/// UUID-named child folder DIRECTLY under the Shared Drive root (a Shared
/// Drive's id doubles as its root folder id) and drives the portable scenarios
/// with a `SharedDrive` context. Unset => those tests print a skip line and
/// pass, exactly like the base gate. See `design/E2E_REAL.md`.
const ENV_SHARED_DRIVE_ID: &str = "DRIVEN_E2E_SHARED_DRIVE_ID";
/// Env var for the OAuth client id (the public installed-app id by default).
const ENV_CLIENT_ID: &str = "DRIVEN_OAUTH_CLIENT_ID";
/// Env var for the OAuth client secret (required to refresh the token).
const ENV_CLIENT_SECRET: &str = "DRIVEN_OAUTH_CLIENT_SECRET";

/// The public installed-app client id (SPEC s4) used when `DRIVEN_OAUTH_CLIENT_ID`
/// is unset. The maintainer minting the token used this client.
const DEFAULT_CLIENT_ID: &str =
    "1094503409775-kvuig3oqtchrq1s4tc1cnpi60mdvnqfe.apps.googleusercontent.com";

/// The resolved real-Drive credentials, or `None` when the gate is closed.
struct E2eCreds {
    refresh_token: String,
    dest_folder_id: String,
    client_id: String,
    client_secret: String,
}

/// Reads the `DRIVEN_E2E_*` gate. Returns `None` (after printing a clear skip
/// line) when either env var is unset, so a credential-less CI run is a
/// no-op pass rather than a failure (ROADMAP M4).
fn e2e_creds(test_name: &str) -> Option<E2eCreds> {
    let refresh_token = std::env::var(ENV_REFRESH_TOKEN)
        .ok()
        .filter(|s| !s.is_empty());
    let dest_folder_id = std::env::var(ENV_DEST_FOLDER_ID)
        .ok()
        .filter(|s| !s.is_empty());
    // The refresh path needs a client secret; the id falls back to the public
    // default. A missing secret closes the gate (we cannot refresh without it).
    let client_secret = std::env::var(ENV_CLIENT_SECRET)
        .ok()
        .filter(|s| !s.is_empty());
    let client_id = std::env::var(ENV_CLIENT_ID)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string());

    match (refresh_token, dest_folder_id, client_secret) {
        (Some(refresh_token), Some(dest_folder_id), Some(client_secret)) => Some(E2eCreds {
            refresh_token,
            dest_folder_id,
            client_id,
            client_secret,
        }),
        _ => {
            eprintln!(
                "skipping real-Drive e2e ({test_name}): set {ENV_REFRESH_TOKEN} + {ENV_DEST_FOLDER_ID} + {ENV_CLIENT_SECRET} to run"
            );
            None
        }
    }
}

/// Builds a live [`GoogleDriveStore`] from the gated credentials and a fresh
/// UUID-named child folder under the dest folder, returning the store + the
/// child folder id the scenario should operate under (ROADMAP M4: "each test
/// uses a UUID-named child folder under the dest folder and cleans up").
async fn setup_store(
    creds: &E2eCreds,
    dest_folder_id: &str,
    drive_context: &DriveContext,
) -> (GoogleDriveStore, String) {
    let ca = driven_drive::CustomCaConfig::none();
    let proxy = driven_drive::ProxyConfig::system();
    let token_source = RefreshingTokenSource::from_stored_refresh_token(
        creds.refresh_token.clone(),
        creds.client_id.clone(),
        creds.client_secret.clone(),
        &ca,
        &proxy,
    )
    .expect("build refreshing token source");
    let store = GoogleDriveStore::with_default_clients(token_source, &ca, &proxy)
        .expect("build GoogleDriveStore");

    // Each test operates inside a fresh UUID-named child folder so concurrent
    // runs never collide and cleanup is a single trash of that subtree
    // (ROADMAP M4). `drive_context` scopes the folder search/create to My Drive
    // or the Shared Drive under test (issue #7).
    let child_name = format!("driven-e2e-{}", uuid::Uuid::new_v4());
    let child = store
        .ensure_folder(dest_folder_id, &child_name, drive_context)
        .await
        .expect("create per-test child folder under the dest folder");
    (store, child.id)
}

/// Cleans up the per-test UUID child folder after the scenario (success or
/// failure), per ROADMAP M4. Trashing the folder removes its whole subtree;
/// a failure to clean up is logged but does not fail the test (the run-scoped
/// dest folder can be swept manually).
async fn teardown_store(store: &GoogleDriveStore, child_folder_id: &str) {
    if let Err(e) = store.trash(child_folder_id).await {
        eprintln!("warning: failed to trash per-test child folder {child_folder_id}: {e}");
    }
}

/// Resolves the base gate for a My Drive run: build the live store + per-test
/// child folder under the My Drive dest folder. `None` when the gate is closed.
async fn gated_store_my_drive(test_name: &str) -> Option<(GoogleDriveStore, String, DriveContext)> {
    let creds = e2e_creds(test_name)?;
    let (store, root) = setup_store(&creds, &creds.dest_folder_id, &DriveContext::MyDrive).await;
    Some((store, root, DriveContext::MyDrive))
}

/// Resolves the Shared Drive gate (issue #7): the base creds PLUS
/// `DRIVEN_E2E_SHARED_DRIVE_ID`. The per-test child folder is created directly
/// under the Shared Drive root (its id doubles as the root folder id) and every
/// scenario runs with a `SharedDrive` context. `None` (with a skip line) when
/// either the base gate or the Shared Drive id is unset.
async fn gated_store_shared_drive(
    test_name: &str,
) -> Option<(GoogleDriveStore, String, DriveContext)> {
    let creds = e2e_creds(test_name)?;
    let Some(shared_drive_id) = std::env::var(ENV_SHARED_DRIVE_ID)
        .ok()
        .filter(|s| !s.is_empty())
    else {
        eprintln!("skipping Shared Drive e2e ({test_name}): set {ENV_SHARED_DRIVE_ID} to run");
        return None;
    };
    let ctx = DriveContext::SharedDrive {
        drive_id: shared_drive_id.clone(),
    };
    // Create the per-test child directly under the Shared Drive root.
    let (store, root) = setup_store(&creds, &shared_drive_id, &ctx).await;
    Some((store, root, ctx))
}

/// The signature every scenario closure satisfies: it receives the live store,
/// the per-test child folder id, and the drive context to scope list/search
/// calls (issue #7).
type ScenarioFn = for<'a> fn(
    &'a GoogleDriveStore,
    &'a str,
    &'a DriveContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>>;

/// Runs `scenario` against an already-resolved gated store, then ALWAYS trashes
/// the per-test child folder - on success AND on a scenario panic (ROADMAP M4).
/// A scenario panic is captured via `catch_unwind`, the folder is trashed, then
/// re-raised so the test still fails.
async fn run_resolved(
    resolved: Option<(GoogleDriveStore, String, DriveContext)>,
    scenario: ScenarioFn,
) {
    let Some((store, root, ctx)) = resolved else {
        return;
    };
    // The future borrows `store`, `root`, and `ctx`, is fully awaited here, and
    // dropped before teardown reuses them - so `AssertUnwindSafe` is sound.
    let fut = std::panic::AssertUnwindSafe(scenario(&store, &root, &ctx));
    let result = futures::FutureExt::catch_unwind(fut).await;
    teardown_store(&store, &root).await;
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

/// Runs `scenario` against the My Drive gate (no-op pass when the gate is
/// closed).
async fn run_gated(test_name: &str, scenario: ScenarioFn) {
    run_resolved(gated_store_my_drive(test_name).await, scenario).await;
}

/// Runs `scenario` against the Shared Drive gate (issue #7; no-op pass when the
/// base gate or `DRIVEN_E2E_SHARED_DRIVE_ID` is unset).
async fn run_gated_shared(test_name: &str, scenario: ScenarioFn) {
    run_resolved(gated_store_shared_drive(test_name).await, scenario).await;
}

// The portable scenario adapters: those that take a drive context thread it;
// the rest ignore it. As `fn` items (not closures) so they satisfy `ScenarioFn`.
fn run_round_trip<'a>(
    s: &'a GoogleDriveStore,
    r: &'a str,
    c: &'a DriveContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(common::scenario_round_trip(s, r, c))
}
fn run_duplicate<'a>(
    s: &'a GoogleDriveStore,
    r: &'a str,
    c: &'a DriveContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(common::scenario_duplicate_names_create(s, r, c))
}
fn run_find_by_op_uuid<'a>(
    s: &'a GoogleDriveStore,
    r: &'a str,
    c: &'a DriveContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(common::scenario_find_by_op_uuid(s, r, c))
}
fn run_update<'a>(
    s: &'a GoogleDriveStore,
    r: &'a str,
    _c: &'a DriveContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(common::scenario_update_preserves_id_merges_props(s, r))
}
fn run_resumable<'a>(
    s: &'a GoogleDriveStore,
    r: &'a str,
    _c: &'a DriveContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(common::scenario_resumable_round_trip(s, r))
}
fn run_resumable_reject<'a>(
    s: &'a GoogleDriveStore,
    r: &'a str,
    _c: &'a DriveContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(common::scenario_resumable_non_multiple_rejected(s, r))
}
fn run_trash<'a>(
    s: &'a GoogleDriveStore,
    r: &'a str,
    _c: &'a DriveContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(common::scenario_trash_idempotent(s, r))
}
fn run_delete_permanent<'a>(
    s: &'a GoogleDriveStore,
    r: &'a str,
    _c: &'a DriveContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(common::scenario_delete_permanent(s, r))
}

#[tokio::test]
async fn google_round_trip() {
    run_gated("google_round_trip", run_round_trip).await;
}

#[tokio::test]
async fn google_duplicate_names_create() {
    run_gated("google_duplicate_names_create", run_duplicate).await;
}

#[tokio::test]
async fn google_update_preserves_id_merges_props() {
    run_gated("google_update_preserves_id_merges_props", run_update).await;
}

#[tokio::test]
async fn google_resumable_round_trip() {
    run_gated("google_resumable_round_trip", run_resumable).await;
}

#[tokio::test]
async fn google_resumable_non_multiple_rejected() {
    run_gated(
        "google_resumable_non_multiple_rejected",
        run_resumable_reject,
    )
    .await;
}

#[tokio::test]
async fn google_trash_idempotent() {
    run_gated("google_trash_idempotent", run_trash).await;
}

#[tokio::test]
async fn google_find_by_op_uuid() {
    run_gated("google_find_by_op_uuid", run_find_by_op_uuid).await;
}

#[tokio::test]
async fn google_delete_permanent() {
    run_gated("google_delete_permanent", run_delete_permanent).await;
}

// ---------------------------------------------------------------------------
// Issue #7 - the SAME portable scenarios against a real Google Shared Drive,
// gated additionally on DRIVEN_E2E_SHARED_DRIVE_ID. These exercise the
// supportsAllDrives + corpora=drive/driveId/includeItemsFromAllDrives wire
// params end-to-end against live Drive.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn google_shared_drive_round_trip() {
    run_gated_shared("google_shared_drive_round_trip", run_round_trip).await;
}

#[tokio::test]
async fn google_shared_drive_duplicate_names_create() {
    run_gated_shared("google_shared_drive_duplicate_names_create", run_duplicate).await;
}

#[tokio::test]
async fn google_shared_drive_resumable_round_trip() {
    run_gated_shared("google_shared_drive_resumable_round_trip", run_resumable).await;
}

#[tokio::test]
async fn google_shared_drive_find_by_op_uuid() {
    run_gated_shared("google_shared_drive_find_by_op_uuid", run_find_by_op_uuid).await;
}

#[tokio::test]
async fn google_shared_drive_trash_idempotent() {
    run_gated_shared("google_shared_drive_trash_idempotent", run_trash).await;
}
