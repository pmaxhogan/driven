-- Issue #36: restore-by-date / point-in-time via a trash-as-version-store.
--
-- When a source has versioning ENABLED (an opt-in per-source setting stored in
-- the `settings` KV under key `versioning:<source_id>`, NOT a column here - so
-- this migration is purely additive and never touches an existing table), a
-- content change to an already-uploaded file is applied as a CREATE of a NEW
-- Drive object followed by an atomic pointer flip in `file_state` and a trash of
-- the OLD object (SPEC s3 `trash`). The OLD object survives - retrievable by id
-- from Drive's trash - as a prior VERSION. This table records one row per such
-- superseded version so the Restore browser can offer "restore as of <date>".
--
-- Each row is the version that was CURRENT during the half-open window
-- [created_at, superseded_at): `created_at` is the wall-time that version first
-- became current (the old `file_state.last_uploaded_at`), `superseded_at` is when
-- the next version replaced it (== the replacing upload's time). Windows are
-- contiguous and non-overlapping, so "restore as of D" for a still-tracked file
-- picks the row with `created_at <= D < superseded_at` (or the live `file_state`
-- row when `D >= its last_uploaded_at`).
--
-- Data-format note (v1.0.0 stored-format stability): additive CREATE TABLE only.
-- Existing installs gain an empty `file_versions` table on upgrade; versioning is
-- OFF by default, so no existing behaviour changes. This is a `feat` (minor), not
-- a `feat!`.
--
-- `hash_blake3` is the PLAINTEXT BLAKE3 (32 bytes) of the version's bytes so a
-- restore can verify the decrypted plaintext regardless of per-source encryption
-- (mirrors `file_state.hash_blake3`); `drive_md5` is the ciphertext md5 for an
-- encrypted source; `encrypted_remote_path` is the cached remote path for an
-- encrypted source (NULL for a plaintext source). `trashed` is 1 once the OLD
-- Drive object has been moved to trash (best-effort after the atomic flip; a
-- reconcile sweep retries any left at 0). `ON DELETE CASCADE` drops a source's
-- versions with it (`PRAGMA foreign_keys` is ON in the pool).
CREATE TABLE file_versions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id TEXT NOT NULL REFERENCES backup_sources(id) ON DELETE CASCADE,
  relative_path TEXT NOT NULL,
  drive_file_id TEXT NOT NULL,
  size INTEGER NOT NULL,
  hash_blake3 BLOB NOT NULL,          -- 32 bytes, plaintext hash
  drive_md5 BLOB,                     -- 16 bytes; ciphertext md5 if encrypted
  encrypted_remote_path TEXT,         -- cached remote path for encrypted sources
  created_at INTEGER NOT NULL,        -- when this version first became current (unix ms)
  superseded_at INTEGER NOT NULL,     -- when it was replaced (unix ms)
  trashed INTEGER NOT NULL DEFAULT 0  -- 1 once the old Drive object is trashed
);

-- The Restore "as of <date>" resolution + the newest-first version listing both
-- key on (source_id, relative_path) ordered by the version window; the
-- count-cap prune walks the same order. A single covering index serves all three.
CREATE INDEX idx_file_versions_path
  ON file_versions(source_id, relative_path, superseded_at DESC);

-- Every versioned change / prune candidate / reconcile-sweep entry looks a Drive
-- object up BY id: `mark_version_trashed` (file_versions) after a best-effort
-- trash, and `drive_file_id_is_live` (file_state, the global no-live-pointer
-- guard) before every trash / hard-delete. Without these indexes both queries
-- full-scan their table on every such call, so a large backup degrades to
-- minutes of pure table scanning per sync cycle (growing with backup size).
-- Amended into this unreleased migration (0006 has never shipped) so both
-- lookups are index-backed from the versioning feature's first release; the
-- `file_state` index is additive over the table created in 0001.
CREATE INDEX idx_file_versions_drive_file_id
  ON file_versions(drive_file_id);
CREATE INDEX idx_file_state_drive_file_id
  ON file_state(drive_file_id);
