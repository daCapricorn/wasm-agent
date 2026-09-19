#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
cargo fmt --check
cargo fmt --manifest-path modules/market/Cargo.toml --check
cargo fmt --manifest-path modules/agent/Cargo.toml --check
bash scripts/build-market.sh
bash scripts/build-agent.sh
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --manifest-path modules/agent/Cargo.toml --locked --all-targets -- -D warnings
cargo test --locked -- --test-threads=1
