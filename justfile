set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

default: dev

dev:
    cargo tauri dev

dev-seeded:
    cargo run --bin seed-fixtures
    $env:DRIVEN_USE_FAKE_REMOTE="1"; cargo tauri dev

test:
    cargo test --workspace
    just _ui-test

_ui-test:
    pnpm --dir ui test:unit

test-e2e-fake:
    cargo test --test e2e_fake

test-e2e-real:
    cargo test --test e2e_real -- --include-ignored

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
    $env:DRIVEN_CHAOS_SOAK="1"; cargo run -p driven-chaos -- run-all --hermetic
    cargo run -p driven-chaos -- fuzz {{args}}

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
    Remove-Item -Recurse -Force ui/dist, ui/node_modules -ErrorAction SilentlyContinue
