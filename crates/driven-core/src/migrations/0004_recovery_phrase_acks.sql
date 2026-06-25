-- M9 R4-P1-1 (M6 R4-P1-1, DATA-SAFETY): DURABLE recovery-phrase ACK gate.
--
-- The FIRST encrypted source for an account generates the account master key
-- and the BIP39 recovery phrase. Until the user has the phrase durably saved,
-- that source must stay DISABLED (so the scheduler + manual sync, which filter
-- on `enabled`, never back it up) and the phrase must stay re-revealable. Before
-- this table the gate state lived ONLY in process memory (app_state
-- `recovery_acks`), so a crash AFTER the source + master key were persisted but
-- BEFORE reveal+ack lost the gate: the user could no longer reveal/ack the
-- phrase and later encrypted sources could arm encryption without it
-- (unrestorable backups).
--
-- This table persists one pending-ack record per first-encrypted-source. It is
-- written IN THE SAME TRANSACTION as the source insert + master-key stamp
-- (`insert_first_encrypted_source_pending_ack`), so a durable encrypted source
-- can NEVER exist without its durable pending-ack record. On startup the
-- in-memory gate is reconstructed from this table. The row is updated to
-- `revealed=1` when `reveal_recovery_phrase` durably records a real backend
-- reveal, and DELETED (in the same transaction that enables the source) when
-- `ack_recovery_phrase_saved` succeeds.
--
-- `source_id` is the PK + an FK with ON DELETE CASCADE, so removing a source
-- drops its pending-ack row automatically (a pending source the user removes
-- before acking leaves no orphan record).
CREATE TABLE recovery_phrase_acks (
  source_id  TEXT PRIMARY KEY REFERENCES backup_sources(id) ON DELETE CASCADE,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  revealed   INTEGER NOT NULL DEFAULT 0,  -- 1 once the backend actually revealed the phrase
  created_at INTEGER NOT NULL             -- unix epoch ms, for diagnostics/ordering
);
