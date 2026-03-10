#!/bin/bash
set -e

cd "$(dirname "$0")/executor"
echo "Building..."
cargo build --release --example full_pipeline
cp target/release/examples/full_pipeline .
echo "Done! Run: cd executor && RUST_LOG=info ./full_pipeline"
