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
