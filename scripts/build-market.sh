#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
if [[ -z "${WASI_SDK_PATH:-}" && -d "$root/.tools/wasi-sdk-34.0-arm64-macos" ]]; then
    WASI_SDK_PATH="$root/.tools/wasi-sdk-34.0-arm64-macos"
fi
if [[ -n "${WASI_SDK_PATH:-}" ]]; then
    export CC_wasm32_wasmer_wasi="$WASI_SDK_PATH/bin/clang"
    export AR_wasm32_wasmer_wasi="$WASI_SDK_PATH/bin/llvm-ar"
fi
cd "$root/modules/${1:-market}"
if [[ -n "${CARGO_WASIX_BIN:-}" ]]; then
    "$CARGO_WASIX_BIN" wasix build --release --locked
elif command -v cargo-wasix >/dev/null 2>&1; then
    cargo wasix build --release --locked
elif [[ -x "$root/.tools/cargo-wasix-aarch64-apple-darwin/cargo-wasix" ]]; then
    "$root/.tools/cargo-wasix-aarch64-apple-darwin/cargo-wasix" wasix build --release --locked
else
    echo 'Install cargo-wasix 0.1.28 first: cargo install cargo-wasix --version 0.1.28 --locked' >&2
    exit 1
fi
