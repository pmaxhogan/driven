# Recipes are POSIX-sh (the primary dev machine is macOS; CI is sh-based).
# On Windows run the underlying commands directly, or use git-bash/WSL -
# the old `windows-shell := powershell` setting is gone because it made
# every env-prefixed recipe (`FOO=1 cmd`) un-runnable on unix and vice versa.

default: dev

dev:
    cargo tauri dev

# Run the app against the in-memory fake remote (no Drive account needed).
# Replaces the stale `dev-seeded` recipe, which invoked a `seed-fixtures`
# binary that no longer exists.
dev-fake:
    DRIVEN_USE_FAKE_REMOTE=1 cargo tauri dev

test:
    cargo test --workspace
    just _ui-test

_ui-test:
    pnpm --dir ui test:unit

test-e2e-fake:
    cargo test --test e2e_fake

# Real Google Drive end-to-end suite (driven-drive/tests/google_e2e.rs). It is
# ENV-GATED (not #[ignore]d): it no-ops unless the DRIVEN_E2E_* creds are set.
# See design/E2E_REAL.md for the required env (refresh token + dest folder, or a
# BYO client id/secret). Same suite the tags-only chaos-real-drive CI job runs.
test-e2e-real:
    cargo test -p driven-drive --test google_e2e -- --nocapture

watch:
    cargo watch -x "test -p driven-core" -x "test -p driven-drive"

# --- chaos / stress harness (design/STRESS_HARNESS.md, ROADMAP M3.7) ---

# Run the full hermetic chaos harness: every scenario, capability-gated.
# Rows whose requires() the host cannot satisfy (admin / VSS / real-Drive /
# wrong-OS) SKIP cleanly; exit 0 = all pass/skip, 1 = any fail.
chaos:
    cargo run -p driven-chaos -- run-all --hermetic

# The dedicated fault-injection subset (s3.7 / s4.2 / s5) - the same set the
# CI `chaos-fake-drive` gate runs. Faster than the full hermetic sweep.
chaos-fake-drive:
    cargo run -p driven-chaos -- run-all --fault-injection

# Remove every cached chaos fixture under target/chaos-fixtures/ so the next
# run rebuilds the big (million-files-nested / huge-file) fixtures from scratch.
chaos-fixture-clean:
    cargo run -p driven-chaos -- fixture clean --all

# Seeded continuous-mutation fuzz soak (STRESS_HARNESS s4.3). `--duration` now
# governs by WALL-CLOCK, so the run actually soaks for the whole duration.
# Override it, e.g. `just chaos-fuzz "--seed 42 --duration 30m"`. An invariant
# violation writes target/chaos-fuzz-failures/<seed>.json for replay.
chaos-fuzz args="--duration 2m":
    cargo run -p driven-chaos -- fuzz {{args}}

# Full local soak - the heavy run the CI cron used to do, now local-only to
# save Actions budget: the soak-gated massive-input rows (million-files-nested,
# tiny-files-100k) plus a long seeded fuzz. Override the fuzz duration via the
# arg, e.g. `just chaos-soak "--duration 6h"`.
chaos-soak args="--duration 30m":
    DRIVEN_CHAOS_SOAK=1 cargo run -p driven-chaos -- run-all --hermetic
    cargo run -p driven-chaos -- fuzz {{args}}

# --- UI visual regression (ui/e2e-visual/README.md) ---

# Run the Playwright visual suite against the committed linux baselines.
# On a non-linux host the first run generates LOCAL (gitignored) baselines -
# advisory only; CI/linux is the gate.
visual:
    pnpm --dir ui run test:visual

# Regenerate the committed LINUX screenshot baselines via the official
# Playwright Docker image, so baselines stay linux-rendered no matter the
# host. The anonymous node_modules volume shadows the host tree - esbuild,
# rollup and @tailwindcss/oxide ship platform-native binaries that hard-fail
# inside linux - so the container installs its own; the second volume keeps
# pnpm's fallback store out of the bind mount.
visual-update:
    docker run --rm \
      -v "$(pwd)":/work -v /work/ui/node_modules -v /work/.pnpm-store -w /work/ui \
      mcr.microsoft.com/playwright:v1.62.1-noble \
      sh -c "corepack enable && pnpm install --frozen-lockfile && pnpm exec playwright test --update-snapshots"

# --- benchmark suite (bench/README.md) ---

# Compare Driven's real engine against rclone on a live Drive account. Uploads
# REAL bytes and takes real time - see bench/README.md for scales and costs.
# `just bench smoke` proves the pipeline in a few minutes; the default `small`
# scale uploads ~610 MiB per tool. Needs rclone on PATH and the DRIVEN_E2E_*
# credentials (the gitignored .env.test at the repo root is loaded for you).
bench scale="small" args="":
    cargo run --release -p driven-bench -- run --scale {{scale}} {{args}}

# Materialise a benchmark fixture without uploading anything, e.g.
# `just bench-fixture tiny-deep small`.
bench-fixture shape scale="small":
    cargo run --release -p driven-bench -- fixture build --shape {{shape}} --scale {{scale}}

# Delete every cached benchmark fixture under target/bench-fixtures/ (the `full`
# scale leaves ~10 GB behind).
bench-fixture-clean:
    cargo run -p driven-bench -- fixture clean

lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    just _ui-lint

_ui-lint:
    pnpm --dir ui lint

fmt:
    cargo fmt --all
    pnpm --dir ui exec prettier --write "src/**/*.{vue,ts,tsx,json,css}"

bundle:
    cargo tauri build

deny:
    cargo deny check

# Print Rust + UI line-coverage totals locally, matching the coverage CI gate
# (.github/workflows/coverage.yml). Needs `cargo install cargo-llvm-cov`. For
# the exact parsed percentages CI compares against main, run ./scripts/coverage.sh.
coverage:
    cargo llvm-cov --workspace --exclude src-tauri --exclude driven-chaos --exclude driven-bench --summary-only
    pnpm --dir ui run test:coverage

# --- sqlx dev helpers (need `cargo install sqlx-cli`) ---

# Regenerate the committed .sqlx/ offline query cache. Spins up a throwaway
# SQLite db, applies the driven-core migrations, prepares the workspace
# (tests included) against it, then drops it. Run this after changing any
# sqlx::query!/query_as! so CI's SQLX_OFFLINE build keeps resolving.
sqlx-prepare:
    cargo sqlx database create --database-url "sqlite:./.driven-prepare.db?mode=rwc"
    cargo sqlx migrate run --source crates/driven-core/src/migrations --database-url "sqlite:./.driven-prepare.db?mode=rwc"
    cargo sqlx prepare --workspace --database-url "sqlite:./.driven-prepare.db?mode=rwc" -- --all-targets
    cargo sqlx database drop -y --database-url "sqlite:./.driven-prepare.db?mode=rwc"

# Apply the driven-core migrations to a given database URL.
# Example: just migrate "sqlite:./state.db?mode=rwc"
migrate db_url:
    cargo sqlx migrate run --source crates/driven-core/src/migrations --database-url "{{db_url}}"

# Drop the local sqlx-prepare scratch db if a previous run left it behind.
db-reset:
    cargo sqlx database drop -y --database-url "sqlite:./.driven-prepare.db?mode=rwc"

clean:
    cargo clean
    rm -rf ui/dist ui/node_modules
