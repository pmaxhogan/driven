-- Issue #7: per-source Google Shared Drive destination.

-- A backup source's destination folder (`drive_folder_id`) can live either in
-- the account's My Drive or inside a Google Shared Drive. This column records
-- which Shared Drive it belongs to so every Drive list/search for the source is
-- scoped correctly (`corpora=drive` + `driveId` + `includeItemsFromAllDrives`);
-- `supportsAllDrives=true` is sent on every request regardless.
--
-- Additive, backward compatible: nullable TEXT. NULL (every pre-migration row)
-- and the sentinel 'my-drive' both decode to My Drive; any other value is the
-- Shared Drive's `driveId`. Existing sources keep uploading to My Drive with no
-- data migration.
--
-- Data-format note (v1.0.0 stored-format stability): additive ADD COLUMN only,
-- no existing column touched.
ALTER TABLE backup_sources ADD COLUMN drive_id TEXT;
