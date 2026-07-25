# Driven benchmark suite

Measures Driven's real backup engine against [rclone](https://rclone.org/) on the
two workloads that dominate real backup sets, and writes a report you can put in
a release note.

The suite exists to answer one question honestly: **is Driven constrained by the
hardware (CPU, disk, network, the Drive API) or by its own algorithms?** A
competitor that is doing roughly the same work is the cheapest way to tell those
apart, and it catches the regressions a synthetic microbenchmark never would.

- Harness crate: [`crates/driven-bench`](../crates/driven-bench)
- Reports land in [`bench/results/`](results/)
- CI workflow: [`.github/workflows/bench.yml`](../.github/workflows/bench.yml)

## What it measures

Two fixture shapes:

| Shape | What it is | What it stresses |
| --- | --- | --- |
| `huge` | A few very large files, flat | Raw upload throughput, chunking, the resumable path |
| `tiny-deep` | Up to a million small files, nested 8 directories deep | Walking, hashing, per-file bookkeeping, request round-trips |

Two phases per shape, in order, against the same destination:

1. **cold** - the tree has never been uploaded. Everything transfers.
2. **incremental** - 0.1% of the files are rewritten deterministically, then the
   same command runs again. Almost nothing should transfer; what is being
   measured is how fast each tool can work out that nothing changed.

Per phase the report records wall-clock time, throughput in MiB/s and files/s,
bytes and files transferred, child-process CPU time, peak working set, the
concurrency the tool ran at, and - for Driven only - the number of Drive API
requests.

## Prerequisites

- **rclone on `PATH`** (or `--rclone <path>`). Windows: `choco install rclone`, or
  unzip the official build from <https://rclone.org/downloads/>. Linux:
  `sudo apt-get install -y rclone`. Any recent version works; the report records
  which one ran.
- **Credentials** for the dedicated automation Google account, in the
  environment:

  | Variable | Purpose |
  | --- | --- |
  | `DRIVEN_E2E_REFRESH_TOKEN` | Google refresh token with `drive` scope |
  | `DRIVEN_E2E_DEST_FOLDER_ID` | The folder every run writes beneath |
  | `DRIVEN_OAUTH_CLIENT_ID` | The OAuth client the token was minted with |
  | `DRIVEN_OAUTH_CLIENT_SECRET` | Its secret |

  Locally these live in the gitignored `.env.test` at the repo root, which the
  harness loads automatically - you do not need to source it yourself. Existing
  environment variables always win, so a CI secret is never shadowed by a stale
  local file. In CI they are repository secrets. These are the same four
  variables the real-Drive e2e suite uses (`design/E2E_REAL.md`).

- **Disk**: fixtures are cached under `target/bench-fixtures/`. The `full` scale
  needs roughly 10 GB there. `just bench-fixture-clean` reclaims it.

## Running it

```powershell
# The usual local run: ~1.2 GB uploaded, both tools, both shapes.
just bench

# Prove the pipeline works without waiting: a few hundred MB.
just bench smoke

# One shape only, and keep the uploaded folder for inspection.
cargo run -p driven-bench -- run --scale small --shape tiny-deep --keep-remote

# The shapes the suite is really about. Needs --full to clear the upload cap.
cargo run -p driven-bench -- run --scale full --full
```

`bench/run.ps1` is the same thing with a friendlier front end
(`.\bench\run.ps1 -Scale smoke`).

### Scales

| Scale | `huge` | `tiny-deep` | Uploaded per tool | Rough duration (both tools) |
| --- | --- | --- | --- | --- |
| `smoke` | 2 x 8 MiB | 300 files | ~17 MiB | a few minutes |
| `small` (default) | 4 x 128 MiB | 50,000 files | ~610 MiB | 20-60 minutes |
| `medium` | 4 x 512 MiB | 200,000 files | ~2.4 GiB | a few hours |
| `full` | 4 x 2 GiB | 1,000,000 files | ~10 GiB | most of a day |

Durations depend almost entirely on your uplink and on Drive's per-file rate
limits; the tiny-files shapes are bound by request rate, not bandwidth, so they
take far longer than their size suggests.

**Cost.** Everything uploaded is trashed at the end of the run, so the storage
cost is transient, but the bytes still cross your connection twice per tool
(once per tool, cold) and count against the account's Drive API quota. Do not
run `full` on a metered connection.

## Safety rails

The suite writes to a real Drive account, so:

- The destination folder id must be given explicitly, by `--dest` or
  `DRIVEN_E2E_DEST_FOLDER_ID`. There is no default and no discovery step; the
  run aborts before generating a single byte if it is missing.
- Every remote write happens under **one** freshly created `driven-bench-<uuid>`
  folder inside that destination, with a subfolder per scenario (tool x fixture).
  The two phases of a scenario deliberately share that subfolder - and, for
  Driven, one state database - because an incremental run has to see what the
  cold run left behind. A fresh folder or a fresh database per phase would make
  the "incremental" numbers a second cold upload wearing the wrong label.
- Cleanup trashes exactly that run folder, **by the id it was created with**. The
  suite never lists the destination folder and never matches anything by name,
  so it cannot touch data it did not create. If cleanup fails it prints the
  folder id to trash by hand rather than retrying blindly.
- Total upload bytes are capped at 2 GiB by default. Exceeding it is an error
  that tells you to pass `--full` or lower `--scale`, rather than silently
  uploading ten times what you expected.
- Credentials are read from the environment only - never from the OS keychain -
  and are never printed. The rclone config containing the token is written to a
  temporary directory that is deleted when the run ends.

## Interpreting the numbers

### What is and is not apples-to-apples

The two tools are given identical input, identical destinations and identical
network conditions. They do **not** do identical work, and pretending otherwise
would make the comparison useless rather than fair:

| | Driven | rclone |
| --- | --- | --- |
| Change detection | Hashes content; maintains a local SQLite state database | Compares size + modification time; no database |
| Cold phase cost | Pays for hashing and state writes on top of the upload | Pays for the upload |
| Incremental phase | Knows exactly what changed, from state | Re-lists the remote and compares every file |
| Crash recovery | Reconciles from `pending_ops` on restart | Re-runs from scratch |
| Concurrency | `min(cores * 2, 16)` by default | 4 transfers by default |
| API requests | Instrumented and reported | Not exposed; the column is blank, which means "not measurable", not zero |

Two consequences worth stating plainly:

- **Driven should be expected to lose, or roughly tie, on the cold `huge`
  phase.** It is doing strictly more work per byte (hashing, state) for benefits
  that only pay off later. If it loses *badly* there, that is a real finding.
- **The incremental phase is where the state database is supposed to earn its
  keep**, especially on `tiny-deep`, where rclone has to re-list a million remote
  objects and Driven does not. If Driven is not clearly ahead there, that is also
  a real finding.

The concurrency defaults differ because each tool runs at its **stock
settings** - that is what a user actually gets. To isolate the algorithms
instead, equalise them with `--rclone-transfers 16` (or whatever
`min(cores * 2, 16)` is on your machine, which the report prints in the `Conc`
column).

### Other caveats

- **Fixture content is incompressible** (a seeded SplitMix64 stream). That is
  deliberate: on zero-filled or text-like data, any tool that compresses on the
  wire - including Driven's own small-file bundling - would post numbers it could
  never reach on the photos, videos and archives that dominate real backup sets.
  It does mean these numbers say nothing about how much compression helps on a
  compressible corpus.
- **The mutation step changes content only** - no creates, no deletes. That keeps
  `rclone copy` a fair match for Driven; a deletion would require `rclone sync`
  to be comparable with Driven's trash pass, and `rclone sync` deletes on the
  remote, which is a materially riskier command to point at a benchmark folder.
- **Driven's numbers come from a real engine run**, not from `driven-cli sync`.
  That subcommand is a debug driver that walks only the top level of a folder and
  keeps no state, so it would upload zero files from the `tiny-deep` fixture. The
  harness assembles the same `SqliteStateRepo` -> `DefaultExecutor` ->
  `SyncOrchestrator` stack `src-tauri/src/assembly.rs` does. Encryption, VSS,
  hooks and network probing are off - they are opt-in features with no rclone
  equivalent, and the probe traffic would pollute the API-call count.
- **Cross-host comparisons are meaningless.** The report always records the OS,
  architecture and CPU count; only compare runs from the same machine and
  connection.
- Both tools run as **child processes**, so CPU time and peak memory come from
  the same OS accounting for both. On Unix, peak RSS is a high-water mark across
  reaped children, so it is reported only when the child raised it; on Windows it
  is exact per process.

### Why rclone, and why not restic

rclone is the right first comparison: it mirrors a local tree to Drive, which is
what Driven does, so the numbers mean the same thing on both sides.

`restic` was considered and deliberately left out. It stores a content-addressed,
chunked, deduplicating repository rather than a mirror of your files, so its
"upload" is a different operation with different outputs - and on a re-run its
deduplication would flatter it on exactly the workload this suite measures. Adding
it would produce a bigger table, not a more honest one. If a chunked-repo
comparison is wanted later it deserves its own scenario and its own caveats
section, not a third column here.

## When it runs

Never on pull requests, and never on a push to `main` - real uploads on every PR
would be slow and expensive. Only:

- **manually**, via `workflow_dispatch` (inputs: `scale`, `tools`), and
- **on `v*` tag pushes**, at the `smoke` scale, as a release-time regression
  check.

The workflow is time-boxed so a hung run cannot burn hours, and it skips cleanly
when the credentials are absent (a fork, or a missing secret) rather than failing
red - the same policy `chaos.yml` uses for its real-Drive job.

## Trending over time

Each run writes `bench/results/<timestamp>.md` and `<timestamp>.json`. The JSON
keeps every field the table omits, so two runs can be diffed without re-running
anything. Results are committed only when you want a durable record; the
directory is otherwise a scratch area.
