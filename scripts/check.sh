#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
cargo fmt --check
cargo fmt --manifest-path modules/market/Cargo.toml --check
bash scripts/build-market.sh
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked -- --test-threads=1
