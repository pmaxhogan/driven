#!/usr/bin/env bash
# coverage.sh - compute Rust + UI line coverage locally, the same way the
# `coverage` CI workflow (.github/workflows/coverage.yml) does. Prints the two
# percentages the coverage gate compares against `main`.
#
# Requires `cargo-llvm-cov` (`cargo install cargo-llvm-cov`) and `jq`. On
# Windows run this under WSL or git-bash.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== Rust (library crates) =="
SQLX_OFFLINE=true cargo llvm-cov --workspace --exclude src-tauri --exclude driven-chaos \
  --summary-only --json --output-path coverage-rust.json
RUST_PCT=$(jq '.data[0].totals.lines.percent' coverage-rust.json)

echo "== UI (vue/ts) =="
pnpm --dir ui run test:coverage >/dev/null
UI_PCT=$(jq '.total.lines.pct' ui/coverage/coverage-summary.json)

echo
printf 'Rust line coverage: %.2f%%\n' "$RUST_PCT"
printf 'UI line coverage:   %.2f%%\n' "$UI_PCT"
echo
echo "These must not drop below main (minus a 0.1pp epsilon) or the coverage gate fails."
