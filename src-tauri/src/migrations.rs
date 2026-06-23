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

use driven_core::state::StateRepo;

/// Open the state DB at `db_path`, running every embedded migration and the
/// integrity check, and return the shared [`StateRepo`] handle the rest of
/// the shell (assembly, IPC) uses.
///
/// TODO(M5): call `SqliteStateRepo::open(db_path).await?`, wrap it
/// `Arc::new(repo) as Arc<dyn StateRepo>`, and return it. Surface a
/// `state.db_corrupt` error verbatim (open already emits it).
pub async fn run(db_path: &Path) -> anyhow::Result<Arc<dyn StateRepo>> {
    let _ = db_path;
    todo!("M5: SqliteStateRepo::open(db_path) -> run migrations + integrity check -> Arc<dyn StateRepo>")
}
