-- V2 small-file bundling (issue #35). Cold folders of many tiny files generate
-- one upload round-trip each, which is slow and burns Google Drive rate limits.
-- Driven can now pack many genuinely-NEW tiny files into a single `.tar.gz`
-- Drive object (a "bundle") and record which member files live inside it.
--
-- This migration is ADDITIVE ONLY - it introduces two new tables and touches no
-- existing table, column, index, trigger, or the `file_state_fts` external-
-- content FTS5 index (that index is keyed on `file_state.rowid`, which we do not
-- alter). Existing per-file Drive objects and their `file_state` rows keep
-- exactly their v1.0.0 shape and restore/reconcile paths, so old data stays
-- fully readable (PR is a plain `feat:`, not a breaking change).
--
-- Data model. A bundled member is represented WITHOUT changing `file_state`:
-- its `file_state` row keeps `drive_file_id = NULL` and `drive_md5 = NULL`
-- (there is no standalone Drive object for the member) plus a `bundle_members`
-- row pointing at the bundle. The invariant is mutual exclusion:
--   a `bundle_members(source_id, relative_path)` row exists  <=>  that member's
--   `file_state.drive_file_id IS NULL` and the bytes live inside `bundle_id`.
-- Keeping the member's `file_state` row otherwise normal means the scanner's
-- (size, mtime) change-detection, the planner's delete handling, and FTS5 search
-- all keep working unchanged: a bundled member that is deleted locally lands in
-- the planner's "no drive_file_id -> just drop the row" branch (no whole-bundle
-- trash), and the composite FK below cascades the membership row away with it.
--
-- Schema-change checklist honoured: new `.sqlx` offline cache regenerated
-- (`just sqlx-prepare`) and both tables added to `KNOWN_STATE_TABLES`
-- (state/mod.rs) so `table_row_count` (diagnostic bundle) accepts them.

-- One row per uploaded bundle object. Inserted only AFTER the `.tar.gz` object
-- lands on Drive and its md5 verifies (transactionally with the member rows in
-- `commit_bundle_result`), so `drive_file_id` is always known and NOT NULL here.
CREATE TABLE bundles (
  id TEXT PRIMARY KEY,                 -- uuid; also carried in appProperties driven.client_op_uuid at upload time
  source_id TEXT NOT NULL REFERENCES backup_sources(id) ON DELETE CASCADE,
  drive_file_id TEXT NOT NULL,         -- Drive file_id of the `.tar.gz` object
  drive_md5 BLOB,                      -- 16 bytes; md5 of the exact bytes stored on Drive (ciphertext md5 if the source is encrypted)
  size INTEGER NOT NULL,              -- byte size of the stored object
  member_count INTEGER NOT NULL,      -- number of members packed at creation (may go cosmetically stale as members leave; restore iterates real bundle_members rows, never this count)
  created_at INTEGER NOT NULL          -- unix epoch ms
);
CREATE INDEX idx_bundles_source ON bundles(source_id);

-- Membership: which member files live inside which bundle. The composite FK to
-- `file_state(source_id, relative_path)` (its PRIMARY KEY) makes SQLite cascade a
-- membership row away when its `file_state` row is deleted (a local deletion, via
-- the planner). The `bundle_id` FK cascades all memberships if a bundle row is
-- ever removed. `foreign_keys = ON` is set on every connection (see sqlite.rs
-- `open`), so both cascades are live.
CREATE TABLE bundle_members (
  source_id TEXT NOT NULL,
  relative_path TEXT NOT NULL,
  bundle_id TEXT NOT NULL REFERENCES bundles(id) ON DELETE CASCADE,
  PRIMARY KEY (source_id, relative_path),
  FOREIGN KEY (source_id, relative_path)
    REFERENCES file_state(source_id, relative_path) ON DELETE CASCADE
);
CREATE INDEX idx_bundle_members_bundle ON bundle_members(bundle_id);
