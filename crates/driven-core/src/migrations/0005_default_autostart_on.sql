-- Default auto-open-on-login to ON (issue #58). The `global` settings group is
-- seeded by migration 0002 with `auto_start_on_login=false`; 0002 is a shipped
-- migration and its `INSERT OR IGNORE` never re-runs, so the default can only be
-- changed by a new, additive migration. This flips the persisted `global` blob's
-- `auto_start_on_login` flag to `true` via SQLite's JSON1 `json_set` (already
-- relied on by 0002's `json_object` seed), making every install - new or
-- existing - default to launching Driven at login. Boot-time reconciliation in
-- the app shell (`reconcile_autostart_on_boot`) then registers the real OS
-- startup entry so the preference actually takes effect.
--
-- This is data-only (no schema/table change), so no `.sqlx` regeneration and no
-- table-list snapshot update is required. It runs exactly once per DB.
--
-- Pre-1.0 note: this intentionally also flips an existing install that had the
-- flag off back to on (the project's pre-1.0 policy accepts this); a user who
-- prefers it off can simply toggle it again in Settings, and that choice
-- persists - this migration never runs a second time.
UPDATE settings
SET value = json_set(value, '$.auto_start_on_login', json('true'))
WHERE key = 'global'
  AND json_extract(value, '$.auto_start_on_login') IS NOT NULL;
