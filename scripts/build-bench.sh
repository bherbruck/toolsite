#!/usr/bin/env bash
# Rebuilds the committed benchmark guest that `cargo bench --bench handler`
# runs. Like the test fixtures it is committed, so the benchmark needs no
# wasm toolchain. Run after changing wit/toolsite.wit or the bench guest.
set -euo pipefail
cd "$(dirname "$0")/.."

rustup target add wasm32-wasip2
cargo build --release \
  --manifest-path tests/fixtures/bench/Cargo.toml \
  --target wasm32-wasip2

cp tests/fixtures/bench/target/wasm32-wasip2/release/toolsite_bench_guest.wasm \
   tests/fixtures/bench.wasm

echo "wrote tests/fixtures/bench.wasm ($(wc -c < tests/fixtures/bench.wasm) bytes)"
