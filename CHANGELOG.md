# Changelog

This file is maintained by [release-please](https://github.com/googleapis/release-please).
Released entries are appended automatically from Conventional Commits when the
"chore: release" pull request is merged. The project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0](https://github.com/pmaxhogan/driven/releases/tag/v0.1.0) (2026-06-25)

Initial general-availability release of Driven, a one-way encrypted backup
desktop app that mirrors local folders into the user's own Google Drive.

### Features

* One-way folder backup to Google Drive. Local additions and changes are
  uploaded; remote files are never written back to disk during normal
  operation, so the source folders are the single source of truth.
* Client-side encryption per source. Filenames and file contents are
  encrypted with XChaCha20-Poly1305 before upload; a BIP39 recovery phrase
  protects the per-account master key. Encryption is opt-in per source.
* Scanner and planner that walk the source tree, honor `.gitignore` plus the
  built-in and user exclude rules, follow the symlink policy, and compute the
  minimal upload / trash plan against the recorded remote state.
* Concurrent executor with a pacer that bounds in-flight work, retries on
  transient network failures, and resumes interrupted multi-part uploads.
* Battery and network awareness. Backups defer on battery and on metered or
  offline networks, and resume automatically when conditions allow.
* Windows Volume Shadow Copy support so files held with an exclusive write
  lock (for example Outlook PST files, running database files) still back up.
* Restore browser. Browse the encrypted remote tree, full-text search file
  names, and restore selected files to a chosen folder with streaming
  decryption.
* Activity dashboard with a live tail and paginated, filterable history of
  every backup operation.
* In-app auto-update with signed update manifests and a stable / dev channel
  selector (Settings > About). See the README for the macOS updater caveat.
* Anonymous, opt-out telemetry (coarse counts only; no file names, paths, or
  content) to help prioritize fixes.
* First-run setup wizard for Google sign-in, bring-your-own OAuth client
  credentials, source selection, and the encryption recovery phrase.
