-- Scheduled integrity scrub of remote objects (driven_core::scrub).
--
-- Driven already re-checks local bit-rot (the weekly deep-verify re-hash) and
-- remote EXISTENCE (the remote-existence audit, migration-free, added with the
-- `bundles` era). Neither notices an object that still exists but whose STORED
-- BYTES stopped matching what `file_state` / `bundles` claim about them. Drive
-- publishes `md5Checksum` and Driven already records the md5 of the exact bytes
-- it sent, so that comparison is one metadata GET per object and no download.
--
-- This migration is ADDITIVE ONLY: two new tables, four new `settings` keys.
-- It touches no existing table, column, index, trigger, or the
-- `file_state_fts` external-content FTS5 index. Old data keeps exactly its
-- shape, so this is a plain `feat:`, not a breaking change.
--
-- Schema-change checklist: both tables are added to `KNOWN_STATE_TABLES`
-- (state/mod.rs) so `table_row_count` (diagnostic bundle) accepts them. No
-- `.sqlx` regeneration is required because every statement the repo issues
-- against these tables uses runtime `sqlx::query` rather than the
-- compile-checked `query!` macro - the same choice
-- `requeue_file_state_for_reupload` made, for the same reason.

-- Where the rolling scrub stopped, per source. A source can hold hundreds of
-- thousands of objects, so a run checks a bounded SLICE and resumes from here
-- next time; the cursors are EXCLUSIVE lower bounds for a keyset page
-- (`WHERE key > cursor ORDER BY key LIMIT n`), and NULL means "start a fresh
-- lap from the beginning". Files and bundles are two independent populations
-- (a bundled member has `file_state.drive_file_id IS NULL` by the migration
-- 0007 invariant, and its bytes' md5 lives on `bundles.drive_md5`), so each
-- gets its own cursor.
--
-- One row per source, created lazily on the first run. The FK cascades the row
-- away with its source, exactly like every other per-source satellite table.
CREATE TABLE scrub_state (
  source_id TEXT PRIMARY KEY REFERENCES backup_sources(id) ON DELETE CASCADE,
  file_cursor TEXT,                    -- last file_state.relative_path checked; NULL = start of lap
  bundle_cursor TEXT,                  -- last bundles.id checked; NULL = start of lap
  last_scrub_at INTEGER                -- unix epoch ms of the last COMPLETED run
);

-- One row per completed scrub run: the persisted report the UI history panel
-- and `driven-cli scrub` render.
--
-- COUNTS ONLY - deliberately. There is no path column, no Drive id column, and
-- no name column, so surfacing a scrub report can never leak an
-- encrypted-source filename (CONTRIBUTING.md house rules). The per-object
-- detail that does exist lives in `activity_log`, which the diagnostic-bundle
-- redactor already scrubs on export.
--
-- `outcome` is one of 'clean' | 'drift' | 'incomplete'. An 'incomplete' run is
-- one whose live-object enumeration failed: it wrote no repairs and advanced no
-- cursor, and is recorded precisely so that "we could not check" is
-- distinguishable from "we checked and it was fine".
CREATE TABLE scrub_runs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id TEXT NOT NULL REFERENCES backup_sources(id) ON DELETE CASCADE,
  started_at INTEGER NOT NULL,         -- unix epoch ms
  finished_at INTEGER NOT NULL,        -- unix epoch ms
  checked INTEGER NOT NULL,
  ok INTEGER NOT NULL,
  missing INTEGER NOT NULL,
  size_mismatch INTEGER NOT NULL,
  hash_mismatch INTEGER NOT NULL,
  unverifiable INTEGER NOT NULL,
  healed INTEGER NOT NULL,
  healed_bundle_members INTEGER NOT NULL,
  unrecoverable INTEGER NOT NULL,
  deep_checked INTEGER NOT NULL,
  deep_failed INTEGER NOT NULL,
  wrapped INTEGER NOT NULL,            -- 1 when the rolling sweep completed a full lap
  outcome TEXT NOT NULL
);
CREATE INDEX idx_scrub_runs_source_started ON scrub_runs(source_id, started_at DESC);
CREATE INDEX idx_scrub_runs_started ON scrub_runs(started_at DESC);

-- Settings. These are standalone KV keys read straight by `driven-core`
-- (`scrub::load_scrub_config`), the same shape the small-file bundling feature
-- uses, rather than fields inside the `global` blob: the scrub re-reads them
-- per run, so no orchestrator reconfigure is needed when one changes.
--
-- `scrub_enabled` ships TRUE. That is the default-ON exception the codebase
-- reserves for a strict improvement that ships with a kill-switch (cf.
-- `adaptive_parallelism_enabled`, migration 0011): the closest analogue, the
-- weekly deep-verify, is likewise unconditionally on, and an integrity check
-- nobody enables detects nothing. `scrub_deep_sample` ships 0 - the metadata
-- comparison already catches remote-side corruption using Drive's own
-- checksum, so spending the user's bandwidth on downloads is opt-in.
--
-- `INSERT OR IGNORE` (not UPDATE ... WHERE json_extract IS NULL, which is the
-- shape used for keys INSIDE the `global` blob) because these are top-level
-- rows: a value already present must never be clobbered, and the statement is
-- idempotent in intent even though a migration runs exactly once per DB.
INSERT OR IGNORE INTO settings (key, value) VALUES ('scrub_enabled', json('true'));
INSERT OR IGNORE INTO settings (key, value) VALUES ('scrub_interval_secs', json('604800'));
INSERT OR IGNORE INTO settings (key, value) VALUES ('scrub_slice_size', json('500'));
INSERT OR IGNORE INTO settings (key, value) VALUES ('scrub_deep_sample', json('0'));
