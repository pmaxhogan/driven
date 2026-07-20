-- Add the `adaptive_parallelism_enabled` key (DESIGN s11.4.7) to the persisted
-- `global` settings blob, defaulting it ON. The `global` group is seeded by
-- migration 0002, whose `INSERT OR IGNORE` never re-runs, so a new key can only
-- be introduced by a new additive migration (same pattern as 0005). This
-- backfills the key on every install - new or existing - so the persisted blob
-- matches the DTO. The host code ALSO tolerates the key's absence (the
-- `#[serde(default)]` on both `storage::Global` and `GlobalSettings` reads a
-- missing key as `true`), so this migration is belt-and-braces, not correctness-
-- critical; it keeps the on-disk shape complete.
--
-- Only set it when absent so a value is never clobbered (no pre-existing value is
-- possible before this feature, but the guard makes the migration idempotent in
-- intent). Data-only (no schema/table change), so no `.sqlx` regeneration and no
-- table-list snapshot update. Runs exactly once per DB.
UPDATE settings
SET value = json_set(value, '$.adaptive_parallelism_enabled', json('true'))
WHERE key = 'global'
  AND json_extract(value, '$.adaptive_parallelism_enabled') IS NULL;
