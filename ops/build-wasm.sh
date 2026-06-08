#!/usr/bin/env bash
# Rebuild the wasm policy guest(s) and stage the module the host loads at runtime.
# Run after changing any policy-* / wasm-* crate.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --manifest-path wasm-genteel/Cargo.toml --target wasm32-unknown-unknown --release
mkdir -p wasm
cp wasm-genteel/target/wasm32-unknown-unknown/release/wasm_genteel.wasm wasm/genteel.wasm
echo "staged wasm/genteel.wasm ($(wc -c < wasm/genteel.wasm) bytes)"
