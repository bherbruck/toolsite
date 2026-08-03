#!/usr/bin/env bash
# Rebuilds the committed wasm test fixture.
#
# The built artifact is committed so `cargo test` stays hermetic — CI needs no
# wasm toolchain. Run this after changing wit/toolsite.wit or the guest source.
set -euo pipefail
cd "$(dirname "$0")/.."

rustup target add wasm32-wasip2
cargo build --release \
  --manifest-path tests/fixtures/guest/Cargo.toml \
  --target wasm32-wasip2

cp tests/fixtures/guest/target/wasm32-wasip2/release/toolsite_test_guest.wasm \
   tests/fixtures/handler.wasm

echo "wrote tests/fixtures/handler.wasm ($(wc -c < tests/fixtures/handler.wasm) bytes)"
