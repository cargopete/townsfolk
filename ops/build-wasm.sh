#!/usr/bin/env bash
# Rebuild the wasm policy guest(s) and stage the module the host loads at runtime.
# Run after changing any policy-* / wasm-* crate.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --manifest-path wasm-policies/Cargo.toml --target wasm32-unknown-unknown --release
mkdir -p wasm
cp wasm-policies/target/wasm32-unknown-unknown/release/wasm_policies.wasm wasm/policies.wasm
echo "staged wasm/policies.wasm ($(wc -c < wasm/policies.wasm) bytes)"
