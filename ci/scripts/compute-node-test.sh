#!/bin/bash

# Exits as soon as any line fails.
set -euo pipefail

echo "--- Run rust clippy check"
cargo clippy --all-targets --all-features --locked -- -D warnings

echo "--- Build documentation"
cargo doc --document-private-items --no-deps

echo "--- Run rust failpoints test"
cargo nextest run failpoints  --features failpoints --no-fail-fast

echo "--- Run rust doc check"
cargo test --doc

echo "--- Run rust test with coverage"
cargo llvm-cov nextest --lcov --output-path lcov.info -- --no-fail-fast


