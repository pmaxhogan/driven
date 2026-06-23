//! Real-Drive contract suite (ROADMAP M4), gated on `DRIVEN_E2E_*` env vars.
//!
//! These run the SAME portable [`common`] scenarios as `fake_contract.rs`,
//! but against a live [`GoogleDriveStore`] built from the maintainer's own
//! refresh token (exercising the production OAuth refresh path; ROADMAP M4).
//! When `DRIVEN_E2E_REFRESH_TOKEN` + `DRIVEN_E2E_DEST_FOLDER_ID` are absent
//! the suite prints a clear "skipping real-Drive e2e" line and returns Ok
//! WITHOUT failing, so CI on a machine with no creds stays green.
//!
//! M4 scaffold: the gate-skip harness is real; building the store from the
//! refresh token (and the per-test UUID-named child folder + cleanup ROADMAP
//! M4 calls for) is `todo!()` for the implement phase.

use driven_drive::google::GoogleDriveStore;

mod common;

/// Env var carrying the maintainer's Google refresh token (ROADMAP M4).
const ENV_REFRESH_TOKEN: &str = "DRIVEN_E2E_REFRESH_TOKEN";
/// Env var carrying the destination Drive folder id the tests upload under
/// (ROADMAP M4; each test uses a UUID-named child of it).
const ENV_DEST_FOLDER_ID: &str = "DRIVEN_E2E_DEST_FOLDER_ID";

/// The resolved real-Drive credentials, or `None` when the gate is closed.
struct E2eCreds {
    refresh_token: String,
    dest_folder_id: String,
}

/// Reads the `DRIVEN_E2E_*` gate. Returns `None` (after printing a clear skip
/// line) when either env var is unset, so a credential-less CI run is a
/// no-op pass rather than a failure (ROADMAP M4).
fn e2e_creds(test_name: &str) -> Option<E2eCreds> {
    match (
        std::env::var(ENV_REFRESH_TOKEN),
        std::env::var(ENV_DEST_FOLDER_ID),
    ) {
        (Ok(refresh_token), Ok(dest_folder_id))
            if !refresh_token.is_empty() && !dest_folder_id.is_empty() =>
        {
            Some(E2eCreds {
                refresh_token,
                dest_folder_id,
            })
        }
        _ => {
            eprintln!(
                "skipping real-Drive e2e ({test_name}): set {ENV_REFRESH_TOKEN} + {ENV_DEST_FOLDER_ID} to run"
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
    let _ = (&creds.refresh_token, &creds.dest_folder_id);
    todo!("M4 implement: refresh -> GoogleDriveStore; create a UUID child folder; return (store, child_id)")
}

/// Cleans up the per-test UUID child folder after the scenario (success or
/// failure), per ROADMAP M4.
async fn teardown_store(store: &GoogleDriveStore, child_folder_id: &str) {
    let _ = (store, child_folder_id);
    todo!("M4 implement: trash the per-test child folder")
}

/// Resolves the gate, builds the live store + per-test child folder, and
/// returns them - or `None` when the gate is closed (the per-test caller then
/// returns early). Keeps each test's preamble to one line.
async fn gated_store(test_name: &str) -> Option<(GoogleDriveStore, String)> {
    let creds = e2e_creds(test_name)?;
    Some(setup_store(&creds).await)
}

#[tokio::test]
async fn google_round_trip() {
    let Some((store, root)) = gated_store("google_round_trip").await else {
        return;
    };
    common::scenario_round_trip(&store, &root).await;
    teardown_store(&store, &root).await;
}

#[tokio::test]
async fn google_duplicate_names_create() {
    let Some((store, root)) = gated_store("google_duplicate_names_create").await else {
        return;
    };
    common::scenario_duplicate_names_create(&store, &root).await;
    teardown_store(&store, &root).await;
}

#[tokio::test]
async fn google_update_preserves_id_merges_props() {
    let Some((store, root)) = gated_store("google_update_preserves_id_merges_props").await else {
        return;
    };
    common::scenario_update_preserves_id_merges_props(&store, &root).await;
    teardown_store(&store, &root).await;
}

#[tokio::test]
async fn google_resumable_round_trip() {
    let Some((store, root)) = gated_store("google_resumable_round_trip").await else {
        return;
    };
    common::scenario_resumable_round_trip(&store, &root).await;
    teardown_store(&store, &root).await;
}

#[tokio::test]
async fn google_resumable_non_multiple_rejected() {
    let Some((store, root)) = gated_store("google_resumable_non_multiple_rejected").await else {
        return;
    };
    common::scenario_resumable_non_multiple_rejected(&store, &root).await;
    teardown_store(&store, &root).await;
}

#[tokio::test]
async fn google_trash_idempotent() {
    let Some((store, root)) = gated_store("google_trash_idempotent").await else {
        return;
    };
    common::scenario_trash_idempotent(&store, &root).await;
    teardown_store(&store, &root).await;
}

#[tokio::test]
async fn google_find_by_op_uuid() {
    let Some((store, root)) = gated_store("google_find_by_op_uuid").await else {
        return;
    };
    common::scenario_find_by_op_uuid(&store, &root).await;
    teardown_store(&store, &root).await;
}
