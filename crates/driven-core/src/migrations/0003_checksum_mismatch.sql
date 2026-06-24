-- R2-P1-3 (DESIGN s5.4 lines 498-500): a per-(source, relative_path)
-- CONSECUTIVE checksum-mismatch counter. After the 3rd verified mismatch the
-- executor marks the file_state row `status='corrupt'`, logs, and stops
-- retrying that file (DESIGN: "Three consecutive mismatches on the same file ->
-- mark status='corrupt', log, surface to user").
--
-- The counter lives in its own table (NOT a file_state column) so it survives
-- across the per-attempt pending_ops lifecycle - each upload op enqueues then
-- deletes a fresh pending_ops row, so the counter cannot live there - while
-- keeping the change purely additive: existing `sqlx::query!` call sites and
-- their cached `.sqlx` metadata are untouched (this milestone's executor reads
-- the table via runtime `sqlx::query`, not the compile-checked macro). The
-- executor clears the row on any successful upload (the streak is CONSECUTIVE)
-- and on reaching the corrupt threshold (so a later user edit gets a fresh
-- budget). ON DELETE CASCADE drops it with the parent source.
CREATE TABLE file_checksum_mismatch (
  source_id TEXT NOT NULL REFERENCES backup_sources(id) ON DELETE CASCADE,
  relative_path TEXT NOT NULL,
  count INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (source_id, relative_path)
);
