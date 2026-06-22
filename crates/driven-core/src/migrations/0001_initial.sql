-- SPEC s2 schema. Mirrors the row types in `crates/driven-core/src/state.rs`
-- one-to-one. The `state.rs` row docs cite this migration as the canonical
-- definition.

CREATE TABLE accounts (
  id TEXT PRIMARY KEY,                -- uuid
  email TEXT NOT NULL,
  display_name TEXT,
  state TEXT NOT NULL,                -- 'ok' | 'needs_reauth' | 'disabled'
  encryption_master_key_id TEXT,      -- keychain handle (the key itself is not stored here)
  created_at INTEGER NOT NULL,
  last_synced_at INTEGER
);

CREATE TABLE backup_sources (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  display_name TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  local_path TEXT NOT NULL,
  drive_folder_id TEXT NOT NULL,
  drive_folder_path TEXT NOT NULL,    -- cached display path
  encryption_enabled INTEGER NOT NULL DEFAULT 0,
  wrapped_source_key BLOB,            -- per-source key, encrypted by master key
  respect_gitignore INTEGER NOT NULL DEFAULT 1,
  include_patterns TEXT NOT NULL DEFAULT '[]',  -- JSON array of globs
  exclude_patterns TEXT NOT NULL DEFAULT '[]',
  schedule_json_v2_reserved TEXT,     -- V2 reserved; V1 code never reads this column
  deep_verify_interval_secs INTEGER NOT NULL DEFAULT 604800,
  last_full_scan_at INTEGER,
  last_deep_verify_at INTEGER,
  created_at INTEGER NOT NULL
);

CREATE TABLE file_state (
  source_id TEXT NOT NULL REFERENCES backup_sources(id) ON DELETE CASCADE,
  relative_path TEXT NOT NULL,
  size INTEGER NOT NULL,
  mtime_ns INTEGER NOT NULL,
  hash_blake3 BLOB NOT NULL,          -- 32 bytes, plaintext for encrypted sources
  drive_file_id TEXT,                 -- null until first upload
  drive_md5 BLOB,                     -- 16 bytes; ciphertext md5 if encrypted
  encrypted_remote_path TEXT,         -- cached, for encrypted sources
  status TEXT NOT NULL,               -- 'synced' | 'pending' | 'corrupt' | 'locked' | 'error' | 'excluded_orphan'
  last_uploaded_at INTEGER,
  last_verified_at INTEGER,
  PRIMARY KEY (source_id, relative_path)
);
CREATE INDEX idx_file_state_status ON file_state(source_id, status);

-- External-content FTS5 index over `file_state.relative_path`. See
-- https://sqlite.org/fts5.html section 4.4.3 for the trigger pattern.
CREATE VIRTUAL TABLE file_state_fts USING fts5(
  relative_path,
  content='file_state',
  content_rowid='rowid',
  tokenize='unicode61 remove_diacritics 2'
);

-- Triggers keep the external-content FTS index in sync with `file_state`.
-- Per the SQLite FTS5 docs, an UPDATE is modeled as `delete` + `insert`.
CREATE TRIGGER file_state_ai AFTER INSERT ON file_state BEGIN
  INSERT INTO file_state_fts(rowid, relative_path)
    VALUES (new.rowid, new.relative_path);
END;

CREATE TRIGGER file_state_ad AFTER DELETE ON file_state BEGIN
  INSERT INTO file_state_fts(file_state_fts, rowid, relative_path)
    VALUES ('delete', old.rowid, old.relative_path);
END;

CREATE TRIGGER file_state_au AFTER UPDATE ON file_state BEGIN
  INSERT INTO file_state_fts(file_state_fts, rowid, relative_path)
    VALUES ('delete', old.rowid, old.relative_path);
  INSERT INTO file_state_fts(rowid, relative_path)
    VALUES (new.rowid, new.relative_path);
END;

CREATE TABLE pending_ops (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id TEXT NOT NULL REFERENCES backup_sources(id) ON DELETE CASCADE,
  op_type TEXT NOT NULL,              -- 'upload' | 'trash' | 'resume' | 'verify'
  relative_path TEXT NOT NULL,
  payload_json TEXT NOT NULL,         -- op-specific payload (resumable session url etc.)
  attempts INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  scheduled_for INTEGER NOT NULL,     -- unix epoch ms
  created_at INTEGER NOT NULL
);
CREATE INDEX idx_pending_ops_due ON pending_ops(scheduled_for, source_id);

CREATE TABLE activity_log (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts INTEGER NOT NULL,
  source_id TEXT REFERENCES backup_sources(id) ON DELETE SET NULL,
  level TEXT NOT NULL,                -- 'info' | 'warn' | 'error'
  event_type TEXT NOT NULL,           -- 'scan_done' | 'upload_done' | 'trash_done' | 'paused' | 'error' | ...
  file_count INTEGER,
  bytes INTEGER,
  message TEXT
);
CREATE INDEX idx_activity_ts ON activity_log(ts DESC);

CREATE TABLE settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL                 -- JSON
);
