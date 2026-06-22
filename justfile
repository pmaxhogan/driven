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

clean:
    cargo clean
    Remove-Item -Recurse -Force ui/dist, ui/node_modules -ErrorAction SilentlyContinue
