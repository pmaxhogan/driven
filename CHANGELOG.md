# Changelog

This file is maintained by [release-please](https://github.com/googleapis/release-please).
Released entries are appended automatically from Conventional Commits when the
"chore: release" pull request is merged. The project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2.12.0](https://github.com/pmaxhogan/driven/compare/v2.11.1...v2.12.0) (2026-08-18)


### Features

* **app:** opt-in debug logging mode and safer, richer diagnostic bundles ([#314](https://github.com/pmaxhogan/driven/issues/314)) ([33c281c](https://github.com/pmaxhogan/driven/commit/33c281c8be56e19cabb20d44575c499baacb7b95))
* **core:** allow nested backup sources when the parent excludes the child ([#294](https://github.com/pmaxhogan/driven/issues/294)) ([0b62df9](https://github.com/pmaxhogan/driven/commit/0b62df9dc771a4b8c05cd3b14950d586447f1826))
* **core:** live exclusion pickup and a visible pending-work queue ([#313](https://github.com/pmaxhogan/driven/issues/313)) ([e6427c7](https://github.com/pmaxhogan/driven/commit/e6427c7faea0b2d4f7f5afd24ad84ecb08a47d83))
* live bottleneck indicator on the Activity dashboard ([#311](https://github.com/pmaxhogan/driven/issues/311)) ([2d9d763](https://github.com/pmaxhogan/driven/commit/2d9d763b40320dbb841cac46497c054b2aa2eabc))
* **ui:** folder picker sort/filter/create/rename and exclusions size rollups ([#315](https://github.com/pmaxhogan/driven/issues/315)) ([7e87341](https://github.com/pmaxhogan/driven/commit/7e8734120b6c16f99f072d31fc0861b116c01a8a))


### Bug Fixes

* **app:** never freeze on tray quit during a backup; quitting tray state; honest recovery status ([#312](https://github.com/pmaxhogan/driven/issues/312)) ([f951cde](https://github.com/pmaxhogan/driven/commit/f951cde9e89692e311f4b96183658807bf6d6ed0))
* clear the attention banner after a passing run and make source removal backend-aware ([#310](https://github.com/pmaxhogan/driven/issues/310)) ([6d8e1ab](https://github.com/pmaxhogan/driven/commit/6d8e1ab2bd9cd5a6cae616f23c1d852bdb6451ce))
* **s3:** honest per-source versioning across backends and multipart-upload leak sweep ([#316](https://github.com/pmaxhogan/driven/issues/316)) ([d462592](https://github.com/pmaxhogan/driven/commit/d462592713f70121ffb1af67ccca9e9c8e796bc6))

## [2.11.1](https://github.com/pmaxhogan/driven/compare/v2.11.0...v2.11.1) (2026-08-15)


### Bug Fixes

* republish 2.11.0 as 2.11.1 after a red release gate ([3e206c4](https://github.com/pmaxhogan/driven/commit/3e206c4f7f49a3ef181cc5e49a7919cf791387e0))

## [2.11.0](https://github.com/pmaxhogan/driven/compare/v2.10.1...v2.11.0) (2026-08-14)


### Features

* **ui:** live disk/network throughput graphs + visible upload recovery ([#290](https://github.com/pmaxhogan/driven/issues/290)) ([4fe156c](https://github.com/pmaxhogan/driven/commit/4fe156c8710a9e3f266d72fab92c8ee73497ac44))


### Bug Fixes

* **core:** exempt Zone.Identifier from the ads_skipped warning ([#288](https://github.com/pmaxhogan/driven/issues/288)) ([f37ff68](https://github.com/pmaxhogan/driven/commit/f37ff68a3fdb2cfded8516a0e843afd256fbddd7))
* let a slow add-source wizard finish instead of failing with a phantom disk error ([#291](https://github.com/pmaxhogan/driven/issues/291)) ([1fbba98](https://github.com/pmaxhogan/driven/commit/1fbba9836d67f9c7c79875d98407b316628bc8ed))

## [2.10.1](https://github.com/pmaxhogan/driven/compare/v2.10.0...v2.10.1) (2026-08-14)


### Bug Fixes

* **core:** stream the resumable-upload resume instead of buffering the whole file ([#279](https://github.com/pmaxhogan/driven/issues/279)) ([adc81fa](https://github.com/pmaxhogan/driven/commit/adc81fad148ab664485dab35813f0d075a0bacec))

## [2.10.0](https://github.com/pmaxhogan/driven/compare/v2.9.0...v2.10.0) (2026-08-03)


### Features

* **core:** scheduled restore drills that prove the backup restores ([#269](https://github.com/pmaxhogan/driven/issues/269)) ([7b558a6](https://github.com/pmaxhogan/driven/commit/7b558a6f9a00f017109bb40166821ccbee0622b0))

## [2.9.0](https://github.com/pmaxhogan/driven/compare/v2.8.0...v2.9.0) (2026-08-03)


### Features

* **cli:** headless restore engine, driven-cli restore, chaos restore coverage ([#243](https://github.com/pmaxhogan/driven/issues/243)) ([0e10e56](https://github.com/pmaxhogan/driven/commit/0e10e56939a7cba27c4d4767217a41429af58fae))
* **core:** SSH (SFTP) backup destination ([#260](https://github.com/pmaxhogan/driven/issues/260)) ([9e41b62](https://github.com/pmaxhogan/driven/commit/9e41b626fc66908c518996c1bae3d73d80109cf4))
* **e2e:** containerized WebDriver harness driving the real app, gating releases ([#247](https://github.com/pmaxhogan/driven/issues/247)) ([ebf13c3](https://github.com/pmaxhogan/driven/commit/ebf13c3390587d0025850c76a81ae326ac1279f5))
* **ui:** scripted IPC mock + Playwright visual regression suite ([#244](https://github.com/pmaxhogan/driven/issues/244)) ([8d09d3d](https://github.com/pmaxhogan/driven/commit/8d09d3d7f911fec151aa2570bab3880f0465f748))


### Bug Fixes

* keychain operations panic on Linux inside the async runtime ([#242](https://github.com/pmaxhogan/driven/issues/242)) ([7df0102](https://github.com/pmaxhogan/driven/commit/7df01028fa1ebc059381175f524429547e1e1a00))

## [2.8.0](https://github.com/pmaxhogan/driven/compare/v2.7.0...v2.8.0) (2026-08-02)


### Features

* **ui:** settings sidebar IA ([#240](https://github.com/pmaxhogan/driven/issues/240)) ([8a486be](https://github.com/pmaxhogan/driven/commit/8a486be994e8b810eb61d98aaf181e7c44f0011e))

## [2.7.0](https://github.com/pmaxhogan/driven/compare/v2.6.0...v2.7.0) (2026-08-02)


### Features

* **ui:** explain pauses in-app with a status banner and one-shot bypass ([#235](https://github.com/pmaxhogan/driven/issues/235)) ([04d86a0](https://github.com/pmaxhogan/driven/commit/04d86a03afd3f993921913bd83db7bb2fe4951c2))

## [2.6.0](https://github.com/pmaxhogan/driven/compare/v2.5.1...v2.6.0) (2026-07-31)


### Features

* **ui:** configurable macOS menu bar extra with live backup metrics ([#233](https://github.com/pmaxhogan/driven/issues/233)) ([8134bb2](https://github.com/pmaxhogan/driven/commit/8134bb26fff185bc21af790314d4851fc02be306))

## [2.5.1](https://github.com/pmaxhogan/driven/compare/v2.5.0...v2.5.1) (2026-07-30)


### Bug Fixes

* **build:** ad-hoc sign the macOS bundle so it is not reported as damaged ([#231](https://github.com/pmaxhogan/driven/issues/231)) ([6dd0cf9](https://github.com/pmaxhogan/driven/commit/6dd0cf92d61323b0db4ca5decf4bddf57c4a4f97))
* **chaos:** expect a permission-denied skip on unix in noaccess-file ([#229](https://github.com/pmaxhogan/driven/issues/229)) ([6bb45df](https://github.com/pmaxhogan/driven/commit/6bb45df6092cb7397df25db33c092bc0fe3da4a4))

## [2.5.0](https://github.com/pmaxhogan/driven/compare/v2.4.0...v2.5.0) (2026-07-30)


### Features

* **cli:** import destinations from an existing rclone config ([#213](https://github.com/pmaxhogan/driven/issues/213)) ([baaf7bd](https://github.com/pmaxhogan/driven/commit/baaf7bdfb07f8d1a7a79219f466d370002fd5e66))
* **core:** enable macOS locked-file backup via the APFS snapshot broker ([#201](https://github.com/pmaxhogan/driven/issues/201)) ([ada822e](https://github.com/pmaxhogan/driven/commit/ada822e969613c3663a6e75faff59e15b4b3ceb8))
* **core:** local and removable-folder backup destination ([#212](https://github.com/pmaxhogan/driven/issues/212)) ([c416a24](https://github.com/pmaxhogan/driven/commit/c416a2463ff434c0141f93c3b82fb44c7113241c))
* **core:** macOS APFS snapshot broker for locked files ([#196](https://github.com/pmaxhogan/driven/issues/196)) ([a5f105e](https://github.com/pmaxhogan/driven/commit/a5f105e87677fd946b8d0f0e92dcce505eea4025))
* **core:** pluggable backup destination backends ([#200](https://github.com/pmaxhogan/driven/issues/200)) ([871df59](https://github.com/pmaxhogan/driven/commit/871df59940d42a1cf3d326aa68d89c19303c9452))
* **core:** S3-compatible backup destination ([#207](https://github.com/pmaxhogan/driven/issues/207)) ([37acb03](https://github.com/pmaxhogan/driven/commit/37acb03086367141633864380daa89dc8960c4c3))
* **core:** scheduled integrity scrub of remote objects ([#203](https://github.com/pmaxhogan/driven/issues/203)) ([049c62a](https://github.com/pmaxhogan/driven/commit/049c62a24dcadf27e4481bee7f287e6b081f655e))
* **ui:** guide macOS users to grant Full Disk Access when files are denied ([#216](https://github.com/pmaxhogan/driven/issues/216)) ([aa5327e](https://github.com/pmaxhogan/driven/commit/aa5327e67e52d84de77b222b96f60855d02a3366))


### Bug Fixes

* **ci:** wait for MinIO readiness before the S3 integration suite ([#226](https://github.com/pmaxhogan/driven/issues/226)) ([fff471a](https://github.com/pmaxhogan/driven/commit/fff471ab14a4b0cee1a3f93c58e8c57e2c8b25a1))
* **core:** classify macOS locked and permission-denied opens into the skip-and-report path ([#195](https://github.com/pmaxhogan/driven/issues/195)) ([08d2864](https://github.com/pmaxhogan/driven/commit/08d286403af1fc6d16c2e2f7ac369ef534ff8911))
* **core:** downgrade the APFS helper-dir check from fatal to advisory ([#211](https://github.com/pmaxhogan/driven/issues/211)) ([65010ac](https://github.com/pmaxhogan/driven/commit/65010acad0f0b0e9398f21a023139cd34dd86b60))
* **net:** redact proxy credentials from the diagnostic bundle ([#190](https://github.com/pmaxhogan/driven/issues/190)) ([8e514f3](https://github.com/pmaxhogan/driven/commit/8e514f3ca17f79017e195f353ae4636838fe5e32))
* **net:** redact userinfo from PAC source in logs ([#208](https://github.com/pmaxhogan/driven/issues/208)) ([8692bc0](https://github.com/pmaxhogan/driven/commit/8692bc041a644a6f5f73365fd67b546aa2de9636))
* **net:** refresh stale PAC scripts instead of pinning them for the process ([#191](https://github.com/pmaxhogan/driven/issues/191)) ([18b0d43](https://github.com/pmaxhogan/driven/commit/18b0d4397884f7fa9a6b61a63556504fa0c5ae27))
* **scanner:** route the deep-verify hash through the platform-open helper ([#193](https://github.com/pmaxhogan/driven/issues/193)) ([3af5c65](https://github.com/pmaxhogan/driven/commit/3af5c655eb3ea6b8e7761756f97cc530a3d6339b))
* **ui:** do not offer versioning on destinations that cannot honour it ([#224](https://github.com/pmaxhogan/driven/issues/224)) ([857c8ba](https://github.com/pmaxhogan/driven/commit/857c8ba81e8012910bffc731c5c44bf768fe5d1b))
* **ui:** make the destination step backend-driven and stop copy claiming Drive behaviour ([#219](https://github.com/pmaxhogan/driven/issues/219)) ([9d67765](https://github.com/pmaxhogan/driven/commit/9d677653ed7dade15378b107efd0d7084b299eed))
* **ui:** tear down exclusion-preview listeners lost to an unmount race ([#206](https://github.com/pmaxhogan/driven/issues/206)) ([2656c9f](https://github.com/pmaxhogan/driven/commit/2656c9f8c175ae4ee45a33940ea7547313b267a7))
* **ui:** use a template tray icon on macOS ([#202](https://github.com/pmaxhogan/driven/issues/202)) ([eaefa9a](https://github.com/pmaxhogan/driven/commit/eaefa9a6e09d5765d3fedf8bbf5985237e131113))

## [2.4.0](https://github.com/pmaxhogan/driven/compare/v2.3.0...v2.4.0) (2026-07-26)


### Features

* **cli:** add dump-client-creds to print an account's stored BYO OAuth client ([#166](https://github.com/pmaxhogan/driven/issues/166)) ([4b6070b](https://github.com/pmaxhogan/driven/commit/4b6070b7470b95efec113fb4b2bc55e2f423db03))
* **core:** parallel scan with negation-aware directory pruning ([#169](https://github.com/pmaxhogan/driven/issues/169)) ([3792e47](https://github.com/pmaxhogan/driven/commit/3792e47cd803626994c5ec6bb292121c92e6ecfe))
* **core:** remote-existence audit heals files whose Drive objects vanished ([#171](https://github.com/pmaxhogan/driven/issues/171)) ([5cb8e3a](https://github.com/pmaxhogan/driven/commit/5cb8e3affd2ec1a5d60f79a65cd97298f70bf9f2))
* **core:** run the scan walk at the configured io_priority ([#173](https://github.com/pmaxhogan/driven/issues/173)) ([8f39961](https://github.com/pmaxhogan/driven/commit/8f399617c64f9038367e740659fa4ad8bb2e5354))
* **core:** shape bundle-build file reads with the io_priority setting ([#179](https://github.com/pmaxhogan/driven/issues/179)) ([af8f048](https://github.com/pmaxhogan/driven/commit/af8f0480bd00a2c6fa4169fdb10b1630e498cf0d))
* **core:** shape upload I/O with the io_priority setting ([#176](https://github.com/pmaxhogan/driven/issues/176)) ([badd9c9](https://github.com/pmaxhogan/driven/commit/badd9c9680920e2200b73f19a5e988af8e9dedf1))
* **core:** wire the ioPriority setting to real OS thread priorities ([#170](https://github.com/pmaxhogan/driven/issues/170)) ([51bb1f3](https://github.com/pmaxhogan/driven/commit/51bb1f345d4da98b07df8dc0c96d4de68afa494b))
* persist rolling backend logs and capture frontend console into diagnostics ([#167](https://github.com/pmaxhogan/driven/issues/167)) ([292e221](https://github.com/pmaxhogan/driven/commit/292e2210966eb2038d79c6fd3be00fce43ddcc2f))
* real-world benchmark suite comparing driven with rclone ([#178](https://github.com/pmaxhogan/driven/issues/178)) ([85f6d6a](https://github.com/pmaxhogan/driven/commit/85f6d6a7413b5e8c3b5b32cbd0ea6e23c4eac19e))
* **ui:** instant exclusion-preview re-evaluation from an in-memory tree ([#177](https://github.com/pmaxhogan/driven/issues/177)) ([8f49570](https://github.com/pmaxhogan/driven/commit/8f4957000c54a89f635a7163eecb417b107fcf12))
* **ui:** sticky shell chrome, indeterminate scan progress and navigation cleanup ([#163](https://github.com/pmaxhogan/driven/issues/163)) ([4896336](https://github.com/pmaxhogan/driven/commit/4896336b8796eede4a5655f0de752061caa6f7b1))
* **ui:** transient in-app toast notifications ([#164](https://github.com/pmaxhogan/driven/issues/164)) ([0104a8e](https://github.com/pmaxhogan/driven/commit/0104a8e896d47a375d8f75e1bc7058de82ea1ac5))
* **ui:** warn when include patterns defeat directory pruning ([#162](https://github.com/pmaxhogan/driven/issues/162)) ([6d3b4b2](https://github.com/pmaxhogan/driven/commit/6d3b4b2a4f265754b3f76d2358e6039123d62d63))


### Bug Fixes

* **core:** self-heal a stale drive_file_id when an update hits a definitive 404 ([#168](https://github.com/pmaxhogan/driven/issues/168)) ([8b983b3](https://github.com/pmaxhogan/driven/commit/8b983b35e5d53304eaeee82b52c32eaae011680f))


### Performance Improvements

* **core:** per-directory decision cursor for the exclusion preview + NFC fast path ([#172](https://github.com/pmaxhogan/driven/issues/172)) ([a587fed](https://github.com/pmaxhogan/driven/commit/a587fed8fad9ff9e19e6429f9641ea41754b7c1e))

## [2.3.0](https://github.com/pmaxhogan/driven/compare/v2.2.0...v2.3.0) (2026-07-25)


### Features

* **core:** record a backup_done activity row when a run completes ([#160](https://github.com/pmaxhogan/driven/issues/160)) ([90cde5c](https://github.com/pmaxhogan/driven/commit/90cde5c4971aa5caadb50f19278ecdae1ebeade4))
* **ui:** files-uploaded stat card with sparkline and smoother Activity load-in ([#157](https://github.com/pmaxhogan/driven/issues/157)) ([2d85c99](https://github.com/pmaxhogan/driven/commit/2d85c9922a55ecc2fa05feff9f9f24f3ff85a520))
* **ui:** live streaming folder-tree preview for the exclusion editor ([#158](https://github.com/pmaxhogan/driven/issues/158)) ([47ebe14](https://github.com/pmaxhogan/driven/commit/47ebe141e1f73372442e812c0645e7d9d1cafc7b))


### Bug Fixes

* **telemetry:** count bundled uploads in the anonymous aggregate ([#159](https://github.com/pmaxhogan/driven/issues/159)) ([11af7ea](https://github.com/pmaxhogan/driven/commit/11af7ea802632817a2d45ed825dc1d824b8b9e9e))
* **ui:** label bundle_upload and hook activity event types ([#154](https://github.com/pmaxhogan/driven/issues/154)) ([2998c76](https://github.com/pmaxhogan/driven/commit/2998c761fb1b3e0910e958bd878db6e05cc8db89))
* **ui:** make the backing-up bar a true determinate progress bar ([#155](https://github.com/pmaxhogan/driven/issues/155)) ([eed80a6](https://github.com/pmaxhogan/driven/commit/eed80a6396496afa035c190a50b80c11a6c5018d))

## [2.2.0](https://github.com/pmaxhogan/driven/compare/v2.1.0...v2.2.0) (2026-07-25)


### Features

* **ui:** add an indefinite pause and a paused banner with one-click resume ([#151](https://github.com/pmaxhogan/driven/issues/151)) ([090d70c](https://github.com/pmaxhogan/driven/commit/090d70cb107fc68c65a9c661599794cf878a1459))
* **ui:** last-5m throughput sparkline behind the current-throughput stat ([#153](https://github.com/pmaxhogan/driven/issues/153)) ([e60b27f](https://github.com/pmaxhogan/driven/commit/e60b27fecd05f867ca8752c610b4ddb7315b4f18))


### Bug Fixes

* **core:** make the manual pause actually halt in-flight upload dispatch ([#148](https://github.com/pmaxhogan/driven/issues/148)) ([d984826](https://github.com/pmaxhogan/driven/commit/d984826dddbc789fcb95ae3bc84c88fef0129280))
* **core:** mirror local folder structure on Drive for plaintext sources ([#150](https://github.com/pmaxhogan/driven/issues/150)) ([a5d0673](https://github.com/pmaxhogan/driven/commit/a5d06738afdcfa9775cc1d8f98022d16a8301a5f))
* **ui:** stream live scan progress from the moment Run Now is clicked ([#149](https://github.com/pmaxhogan/driven/issues/149)) ([12ddd1e](https://github.com/pmaxhogan/driven/commit/12ddd1ea17f752b1473e9f8ecc46c24dd652aa6c))

## [2.1.0](https://github.com/pmaxhogan/driven/compare/v2.0.1...v2.1.0) (2026-07-24)


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

* **core:** commit file_state for a create that skipped post-upload so the next scan updates instead of re-creating ([#146](https://github.com/pmaxhogan/driven/issues/146)) ([f5230d1](https://github.com/pmaxhogan/driven/commit/f5230d16e448b219957c315f7b240a3ffe87e4fb))
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
