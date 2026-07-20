# Changelog

This file is maintained by [release-please](https://github.com/googleapis/release-please).
Released entries are appended automatically from Conventional Commits when the
"chore: release" pull request is merged. The project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2.1.0](https://github.com/pmaxhogan/driven/compare/v2.0.1...v2.1.0) (2026-07-20)


### Features

* **core:** adaptive upload parallelism with throughput probe and disk-saturation gate ([#143](https://github.com/pmaxhogan/driven/issues/143)) ([8ecced6](https://github.com/pmaxhogan/driven/commit/8ecced61dcffa2dbaea4aaec8a4f2adb8e33b873))
* **core:** filesystem timestamp-granularity probe with ctime fallback and per-directory gitignore cascade ([#141](https://github.com/pmaxhogan/driven/issues/141)) ([344262c](https://github.com/pmaxhogan/driven/commit/344262c142707aec4b7681c5da904e0cef19ee2e))
* **drive:** support Google Shared Drive destinations end-to-end ([#142](https://github.com/pmaxhogan/driven/issues/142)) ([d9c3161](https://github.com/pmaxhogan/driven/commit/d9c3161cbcd8e7930f55b140365cb771b53892f1))
* **net:** native OS reachability backends with automatic fallback ([#138](https://github.com/pmaxhogan/driven/issues/138)) ([319e85f](https://github.com/pmaxhogan/driven/commit/319e85f2f66258eaa152ab68c0e9cade88bf1cbf))
* **net:** SOCKS5 and PAC proxy support for all outbound connections ([#145](https://github.com/pmaxhogan/driven/issues/145)) ([2f0b7d1](https://github.com/pmaxhogan/driven/commit/2f0b7d13f899d7b5d719508e3b317c8538647565))
* **net:** support a custom corporate root CA for all outbound connections ([#134](https://github.com/pmaxhogan/driven/issues/134)) ([929e93d](https://github.com/pmaxhogan/driven/commit/929e93df72e8b35a4a394eb0d99ab6f90b66201d))
* per-source toggle to back up OneDrive cloud-only placeholder files ([#133](https://github.com/pmaxhogan/driven/issues/133)) ([6863ea3](https://github.com/pmaxhogan/driven/commit/6863ea3db7985301e93955c951efee3186fe465a))
* **telemetry:** capture latency percentiles and add rollup query endpoint ([#132](https://github.com/pmaxhogan/driven/issues/132)) ([4e9fde6](https://github.com/pmaxhogan/driven/commit/4e9fde6b2caea675930ca07b57edc5a6ed5db073))
* **telemetry:** preview exactly what a telemetry ping sends ([#139](https://github.com/pmaxhogan/driven/issues/139)) ([95fbd9a](https://github.com/pmaxhogan/driven/commit/95fbd9a7c670b74e20898f6bd60c7ecd62a0d09b))


### Bug Fixes

* **deps:** bump tauri-winrt-notification to drop vulnerable quick-xml (closes [#89](https://github.com/pmaxhogan/driven/issues/89)) ([#129](https://github.com/pmaxhogan/driven/issues/129)) ([232fd8f](https://github.com/pmaxhogan/driven/commit/232fd8fee62a9bdcdf161165bcd8da55f11b04ed))
* **telemetry:** exclude pre-schema rows from latency rollup ([#137](https://github.com/pmaxhogan/driven/issues/137)) ([1ae6220](https://github.com/pmaxhogan/driven/commit/1ae6220cf59a3fa2be8b46fff8001bfe35ca130a))
* **ui:** add cursor pointer to buttons and link-buttons ([#136](https://github.com/pmaxhogan/driven/issues/136)) ([dbd4809](https://github.com/pmaxhogan/driven/commit/dbd48090cc77d7e09b616274ab54b0894ddc9af1))

## [2.0.1](https://github.com/pmaxhogan/driven/compare/v2.0.0...v2.0.1) (2026-07-20)


### Bug Fixes

* **updater:** shut down the VSS helper before applying an update ([#126](https://github.com/pmaxhogan/driven/issues/126)) ([f1321a8](https://github.com/pmaxhogan/driven/commit/f1321a8b842a17fab29621acce899aa5435b09f4))

## [2.0.0](https://github.com/pmaxhogan/driven/compare/v1.0.0...v2.0.0) (2026-07-19)


### Features

* **core:** pack cold small-file folders into tar.gz bundles ([#110](https://github.com/pmaxhogan/driven/issues/110)) ([95b573a](https://github.com/pmaxhogan/driven/commit/95b573a6203cd156e904cbcb95e3ef8f72554b87))
* **core:** restore-by-date point-in-time restore via trash-as-version-store ([#109](https://github.com/pmaxhogan/driven/issues/109)) ([aed3f11](https://github.com/pmaxhogan/driven/commit/aed3f1183be3da09ca6f923a0ae8d4d9cfa30891))
* least-privilege VSS elevation helper broker + secured IPC ([#96](https://github.com/pmaxhogan/driven/issues/96)) ([d8886be](https://github.com/pmaxhogan/driven/commit/d8886beb7d887939b0ac7bcf760d7f70afa8d180))
* **power:** real metered-network detection on macOS and Linux ([#95](https://github.com/pmaxhogan/driven/issues/95)) ([c475ac1](https://github.com/pmaxhogan/driven/commit/c475ac10178a8bb1f6718eb915ac65e77d46dac5))
* wire OS sleep/wake power events (suspend-edge session snapshot) ([#97](https://github.com/pmaxhogan/driven/issues/97)) ([bcbfed6](https://github.com/pmaxhogan/driven/commit/bcbfed62f0e4de469e2375fd73180ad8064a9847))
* wire the least-privilege VSS helper into locked-file backup ([#112](https://github.com/pmaxhogan/driven/issues/112)) ([70ffbcb](https://github.com/pmaxhogan/driven/commit/70ffbcb2192932c9323e75acb21be0b6e2f8afbb))


### Bug Fixes

* **core:** dedicated restore.no_version_as_of error code for point-in-time rejections ([#111](https://github.com/pmaxhogan/driven/issues/111)) ([64a23b5](https://github.com/pmaxhogan/driven/commit/64a23b505ba4db44cab90afc584a9b5aaefce865))
* **deps:** bump crossbeam-epoch to 0.9.20 and spin to 0.9.9 to clear cargo-deny advisories ([62f3356](https://github.com/pmaxhogan/driven/commit/62f33567309bfd6e3ac28c5efeae2cc55149081e))
* eager VSS helper launch with attended UAC window and decline-only memoisation ([#113](https://github.com/pmaxhogan/driven/issues/113)) ([3c6b029](https://github.com/pmaxhogan/driven/commit/3c6b0295b97c2e0086766e329772283fd331c463))


### Miscellaneous Chores

* release 2.0.0 ([871492f](https://github.com/pmaxhogan/driven/commit/871492f17dff48e81a8d930f8bfb3e7f8139ac3f))

## [1.0.0](https://github.com/pmaxhogan/driven/compare/v0.5.0...v1.0.0) (2026-07-03)


### Features

* adopt v1.0.0 GA versioning and compatibility policy ([#92](https://github.com/pmaxhogan/driven/issues/92)) ([bdff783](https://github.com/pmaxhogan/driven/commit/bdff783d203aed02603086751acf2228eceabedc))


### Bug Fixes

* **chaos:** assert AppendOnlyLog post-restart contract via reconcile ([#86](https://github.com/pmaxhogan/driven/issues/86)) ([1c2da3b](https://github.com/pmaxhogan/driven/commit/1c2da3bc56b340cd3e55b3c34d7ad1566ed389b4))
* **ci:** resolve cargo-deny advisories (anyhow 1.0.103, quick-xml ignores) ([#90](https://github.com/pmaxhogan/driven/issues/90)) ([8d85e62](https://github.com/pmaxhogan/driven/commit/8d85e623224b89b8aa4104a026e9a791d91a8eec))

## [0.5.0](https://github.com/pmaxhogan/driven/compare/v0.4.0...v0.5.0) (2026-06-27)


### Features

* auto-open on startup setting ([#66](https://github.com/pmaxhogan/driven/issues/66)) ([3dbae06](https://github.com/pmaxhogan/driven/commit/3dbae06f0f23751ac2a08854979b2019c676a4ea)), closes [#58](https://github.com/pmaxhogan/driven/issues/58)
* per-state tray glyph icons and animated syncing ([#64](https://github.com/pmaxhogan/driven/issues/64)) ([2c2755f](https://github.com/pmaxhogan/driven/commit/2c2755f1cb38098d2b5aab3bc0203a903982f32a))
* publish Docker images for the CLI and chaos-soak ([#60](https://github.com/pmaxhogan/driven/issues/60)) ([8df71e6](https://github.com/pmaxhogan/driven/commit/8df71e6fbe4ce33d8db22ffb7a76e726b16d2788))

## [0.4.0](https://github.com/pmaxhogan/driven/compare/v0.3.1...v0.4.0) (2026-06-27)


### Features

* **ui:** global backup progress bar in the app header ([#55](https://github.com/pmaxhogan/driven/issues/55)) ([b7513b5](https://github.com/pmaxhogan/driven/commit/b7513b55bf1614bcef1081857389f57118bb8ae2))
* **ui:** sticky restore action bar and virtualized large lists ([#54](https://github.com/pmaxhogan/driven/issues/54)) ([4631225](https://github.com/pmaxhogan/driven/commit/4631225cc7fda29e8348e3bb00915a0a47b81e31))


### Bug Fixes

* **ui:** keep the activity screen smooth during uploads ([#56](https://github.com/pmaxhogan/driven/issues/56)) ([44f3853](https://github.com/pmaxhogan/driven/commit/44f38539f00e0cccf8d4b7501b79cefbf4f41cc2))

## [0.3.1](https://github.com/pmaxhogan/driven/compare/v0.3.0...v0.3.1) (2026-06-26)


### Bug Fixes

* **settings:** allow resetting nullable settings to Auto/Unlimited ([#50](https://github.com/pmaxhogan/driven/issues/50)) ([4108c13](https://github.com/pmaxhogan/driven/commit/4108c13ae4f45d6349c39cb875159b7a245a6347))
* **ui:** onboarding Drive picker, settings error handling, dark titlebar, and activity labels ([#48](https://github.com/pmaxhogan/driven/issues/48)) ([cca1cf9](https://github.com/pmaxhogan/driven/commit/cca1cf986149b5cac43b2512c5cd14ffafa927fd))

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
