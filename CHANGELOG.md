# Changelog

This file is maintained by [release-please](https://github.com/googleapis/release-please).
Released entries are appended automatically from Conventional Commits when the
"chore: release" pull request is merged. The project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0](https://github.com/pmaxhogan/driven/compare/v0.2.0...v0.3.0) (2026-06-26)


### Features

* comprehensive UI/UX overhaul + tray i18n/icon fix + CLI tests ([#37](https://github.com/pmaxhogan/driven/issues/37)) ([542e276](https://github.com/pmaxhogan/driven/commit/542e276c8e1ed14113b49d66b8693bff4505e403))
* **updater:** floor the dev channel to stable so dev never falls behind ([#20](https://github.com/pmaxhogan/driven/issues/20)) ([1ebd52e](https://github.com/pmaxhogan/driven/commit/1ebd52e8e36e7e1faa9d0236155eea5f094d8444))


### Bug Fixes

* **capstone:** recovery-repair must not re-enable user-disabled encrypted sources ([04c9ba9](https://github.com/pmaxhogan/driven/commit/04c9ba9ec032e273ba69250f6a026ee5af23de35))

## [0.2.0](https://github.com/pmaxhogan/driven/compare/v0.1.0...v0.2.0) (2026-06-25)


### Features

* **cli:** local-state inspection subcommands (status / history / verify) ([#13](https://github.com/pmaxhogan/driven/issues/13)) ([4a4763a](https://github.com/pmaxhogan/driven/commit/4a4763a48c6cb1bc1f6eba3824215620066934dd))
* **core:** metered network pause-or-throttle toggle ([#17](https://github.com/pmaxhogan/driven/issues/17)) ([4b690d5](https://github.com/pmaxhogan/driven/commit/4b690d5146c94936ae8659012d0a4a16d35558cd))
* **core:** pre/post backup shell hooks ([#16](https://github.com/pmaxhogan/driven/issues/16)) ([35df924](https://github.com/pmaxhogan/driven/commit/35df9242add4276e93f5affb395bf1542f9d1284))
* **core:** schedule windows (time-of-day backup gating) ([3de2cf7](https://github.com/pmaxhogan/driven/commit/3de2cf76ca4a2afa8485859ecf930ccbc6970cb2))
* **core:** schedule windows (time-of-day backup gating) ([89afd39](https://github.com/pmaxhogan/driven/commit/89afd39f0ade49eff27ba8eed5a2662bf9201c54))
* **landing:** on-brand driven.maxhogan.dev root page + assemble script (M12) ([0809cdc](https://github.com/pmaxhogan/driven/commit/0809cdc308b442e4dc51b210f4faa13d465cba3a))


### Bug Fixes

* **capstone:** post-GA hardening - recovery-repair, OAuth cred validation, restore fail-closed ([4236724](https://github.com/pmaxhogan/driven/commit/4236724eef3685d4346ce8cb3d02c30d18f999fd))
* **ci:** do not cancel push-to-main coverage baseline runs ([#15](https://github.com/pmaxhogan/driven/issues/15)) ([6e26a5d](https://github.com/pmaxhogan/driven/commit/6e26a5dd05b1f9d222468061853e18e653225e7a))
* **ui:** keep vitest coverage config out of vite.config.ts ([e3a353e](https://github.com/pmaxhogan/driven/commit/e3a353e1c62e30bc6cd74f5b8a1ef718c19c6b5d))

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
