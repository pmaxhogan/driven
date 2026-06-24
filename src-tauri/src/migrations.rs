//! Boot-time SQLite migration runner (SPEC s2).
//!
//! `driven-core`'s [`SqliteStateRepo::open`](driven_core::state::sqlite::SqliteStateRepo::open)
//! already applies the embedded `sqlx` migrations and runs
//! `PRAGMA integrity_check` on open. The app shell opens the DB at boot
//! through this one helper so the `.setup()` chain has a single migration +
//! integrity entry point (and a place to seed the runtime-only `windows`
//! settings key per CODEX_NOTES M3.5, when M6 wires the settings UI).

use std::path::Path;
use std::sync::Arc;

use driven_core::state::{SqliteStateRepo, StateRepo};

/// Open the state DB at `db_path`, running every embedded migration and the
/// integrity check, and return the shared [`StateRepo`] handle the rest of
/// the shell (assembly, IPC) uses.
///
/// Delegates to [`SqliteStateRepo::open`], which already applies the embedded
/// `sqlx` migrations and runs `PRAGMA integrity_check` on open (SPEC s2) - the
/// SAME migration source the chaos / handle path uses, so no SQL is duplicated
/// here. A corrupt DB surfaces verbatim as the open error (`state.db_corrupt`),
/// aborting startup rather than syncing against a damaged ledger.
pub async fn run(db_path: &Path) -> anyhow::Result<Arc<dyn StateRepo>> {
    let repo = SqliteStateRepo::open(db_path).await?;
    Ok(Arc::new(repo) as Arc<dyn StateRepo>)
}
