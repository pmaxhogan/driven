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
use driven_drive::remote_store::RemoteStore;

mod common;

/// Env var carrying the maintainer's Google refresh token (ROADMAP M4).
const ENV_REFRESH_TOKEN: &str = "DRIVEN_E2E_REFRESH_TOKEN";
/// Env var carrying the destination Drive folder id the tests upload under
/// (ROADMAP M4; each test uses a UUID-named child of it).
const ENV_DEST_FOLDER_ID: &str = "DRIVEN_E2E_DEST_FOLDER_ID";
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
async fn setup_store(creds: &E2eCreds) -> (GoogleDriveStore, String) {
    let token_source = RefreshingTokenSource::from_stored_refresh_token(
        creds.refresh_token.clone(),
        creds.client_id.clone(),
        creds.client_secret.clone(),
    )
    .expect("build refreshing token source");
    let store =
        GoogleDriveStore::with_default_clients(token_source).expect("build GoogleDriveStore");

    // Each test operates inside a fresh UUID-named child folder so concurrent
    // runs never collide and cleanup is a single trash of that subtree
    // (ROADMAP M4).
    let child_name = format!("driven-e2e-{}", uuid::Uuid::new_v4());
    let child = store
        .ensure_folder(&creds.dest_folder_id, &child_name)
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

/// Resolves the gate, builds the live store + per-test child folder, and
/// returns them - or `None` when the gate is closed (the per-test caller then
/// returns early). Keeps each test's preamble to one line.
async fn gated_store(test_name: &str) -> Option<(GoogleDriveStore, String)> {
    let creds = e2e_creds(test_name)?;
    Some(setup_store(&creds).await)
}

/// Runs `scenario` against the gated store, then ALWAYS trashes the per-test
/// child folder - on success AND on a scenario panic (ROADMAP M4: "cleaning up
/// on success AND failure"). The scenario receives a `&GoogleDriveStore` + the
/// child folder id; `run_gated` retains ownership so it can tear down after.
/// A scenario panic is captured via `catch_unwind`, the folder is trashed,
/// then the panic is re-raised so the test still fails. When the gate is
/// closed this is a no-op pass.
async fn run_gated<F>(test_name: &str, scenario: F)
where
    F: for<'a> FnOnce(
        &'a GoogleDriveStore,
        &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>>,
{
    let Some((store, root)) = gated_store(test_name).await else {
        return;
    };
    // Catch a scenario panic so teardown still runs (the scenarios assert with
    // `expect`/`assert!`, which panic on failure). The future borrows `store`
    // and `root`, is fully awaited here, and dropped before teardown reuses
    // them - so `AssertUnwindSafe` over the borrowed future is sound.
    let fut = std::panic::AssertUnwindSafe(scenario(&store, &root));
    let result = futures::FutureExt::catch_unwind(fut).await;
    teardown_store(&store, &root).await;
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[tokio::test]
async fn google_round_trip() {
    run_gated("google_round_trip", |store, root| {
        Box::pin(common::scenario_round_trip(store, root))
    })
    .await;
}

#[tokio::test]
async fn google_duplicate_names_create() {
    run_gated("google_duplicate_names_create", |store, root| {
        Box::pin(common::scenario_duplicate_names_create(store, root))
    })
    .await;
}

#[tokio::test]
async fn google_update_preserves_id_merges_props() {
    run_gated("google_update_preserves_id_merges_props", |store, root| {
        Box::pin(common::scenario_update_preserves_id_merges_props(
            store, root,
        ))
    })
    .await;
}

#[tokio::test]
async fn google_resumable_round_trip() {
    run_gated("google_resumable_round_trip", |store, root| {
        Box::pin(common::scenario_resumable_round_trip(store, root))
    })
    .await;
}

#[tokio::test]
async fn google_resumable_non_multiple_rejected() {
    run_gated("google_resumable_non_multiple_rejected", |store, root| {
        Box::pin(common::scenario_resumable_non_multiple_rejected(
            store, root,
        ))
    })
    .await;
}

#[tokio::test]
async fn google_trash_idempotent() {
    run_gated("google_trash_idempotent", |store, root| {
        Box::pin(common::scenario_trash_idempotent(store, root))
    })
    .await;
}

#[tokio::test]
async fn google_find_by_op_uuid() {
    run_gated("google_find_by_op_uuid", |store, root| {
        Box::pin(common::scenario_find_by_op_uuid(store, root))
    })
    .await;
}
