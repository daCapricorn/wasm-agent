#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
bash scripts/build-market.sh
cargo build --locked --bin wasm-agent --example mock_feed
./target/debug/examples/mock_feed &
feed_pid=$!
trap 'kill "$feed_pid" 2>/dev/null || true; wait "$feed_pid" 2>/dev/null || true' EXIT
export MARKET_ABOVE=100
./target/debug/wasm-agent --allow-network --env MARKET_ABOVE --duration 5
