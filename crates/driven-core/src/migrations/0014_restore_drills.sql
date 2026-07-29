-- Scheduled restore drills (driven_core::drill).
--
-- Every existing integrity check verifies the backup from the WRITE side: md5
-- after each upload, the deep-verify re-hash, the remote-existence audit, and
-- the integrity scrub added in migration 0013. None of them ever exercises the
-- READ side - and restore is the only operation that runs the full reverse
-- pipeline (download, stream-decrypt with the source key, extract a bundle
-- member, verify the plaintext BLAKE3, write out). A bug or a lost key anywhere
-- in that chain is invisible to every write-side check and shows up exactly
-- once: the day the user needs their data.
--
-- A drill restores a small, deterministically-sampled set of files through that
-- real path into a temp directory, verifies them, deletes the output, and
-- records what happened.
--
-- ADDITIVE ONLY: two new tables, three new `settings` keys. No existing table,
-- column, index, trigger, or the `file_state_fts` external-content FTS5 index is
-- touched. Both tables are added to `KNOWN_STATE_TABLES` (state/mod.rs) so
-- `table_row_count` (diagnostic bundle) accepts them. No `.sqlx` regeneration is
-- required: every statement the repo issues against these tables uses runtime
-- `sqlx::query` rather than the compile-checked `query!` macro.

-- When each source was last drilled. One row per source, created lazily on the
-- first drill; the FK cascades it away with its source.
--
-- Deliberately a separate table from `scrub_state` even though both hold a
-- last-run stamp: the two jobs run on different cadences and either can be
-- disabled independently, so sharing a row would couple their schedules and
-- make "scrub ran, drill did not" unrepresentable.
CREATE TABLE drill_state (
  source_id TEXT PRIMARY KEY REFERENCES backup_sources(id) ON DELETE CASCADE,
  last_drill_at INTEGER                -- unix epoch ms of the last COMPLETED drill
);

-- One row per completed drill: the persisted report the UI panel and
-- `driven-cli drill` render.
--
-- COUNTS plus stable SPEC s24 ERROR CODES - and nothing else. There is no path
-- column, no remote id, and no filename, so surfacing a drill report can never
-- leak an encrypted source's filenames (CONTRIBUTING.md house rules). Error
-- codes are a closed, non-user vocabulary (`drive.unreachable`,
-- `crypto.key_missing`, ...), so `failure_codes` carries no user data either.
--
-- `outcome` is one of 'passed' | 'failed' | 'inconclusive'. 'inconclusive' means
-- nothing was actually verified (no restorable files, or every candidate was
-- skipped because its account's key was unavailable) - recorded distinctly on
-- purpose, because "we restored nothing" must never read as "we restored
-- everything successfully".
CREATE TABLE drill_runs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id TEXT NOT NULL REFERENCES backup_sources(id) ON DELETE CASCADE,
  started_at INTEGER NOT NULL,         -- unix epoch ms
  finished_at INTEGER NOT NULL,        -- unix epoch ms
  sampled INTEGER NOT NULL,
  verified INTEGER NOT NULL,
  skipped INTEGER NOT NULL,
  failed INTEGER NOT NULL,
  failure_codes TEXT NOT NULL,         -- JSON array of [code, count] pairs, sorted by code
  outcome TEXT NOT NULL
);
CREATE INDEX idx_drill_runs_source_started ON drill_runs(source_id, started_at DESC);
CREATE INDEX idx_drill_runs_started ON drill_runs(started_at DESC);

-- Settings. Standalone KV keys read straight by `driven-core`
-- (`drill::load_drill_config`), the same shape the integrity scrub and the
-- small-file bundling feature use: re-read per run, so a change applies from the
-- next cycle with no orchestrator reconfigure.
--
-- `restore_drill_enabled` ships TRUE for the same reason `scrub_enabled` does -
-- a data-safety detector nobody enables detects nothing, and it carries a real
-- kill-switch. The cadence is MONTHLY rather than the scrub's weekly because a
-- drill spends real bandwidth, and the failures it catches (a broken restore
-- path, an unusable key) are systemic rather than per-file, so they do not need
-- weekly sampling to be found. Three files is enough to catch a systemic
-- failure on the first run.
INSERT OR IGNORE INTO settings (key, value) VALUES ('restore_drill_enabled', json('true'));
INSERT OR IGNORE INTO settings (key, value) VALUES ('restore_drill_interval_secs', json('2592000'));
INSERT OR IGNORE INTO settings (key, value) VALUES ('restore_drill_sample_size', json('3'));
