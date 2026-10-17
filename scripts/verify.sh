#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
echo '[1/5] Format'
cargo fmt --all
cargo fmt --all -- --check
echo '[2/5] Type checking'
cargo check --all-targets
echo '[3/5] Clippy'
cargo clippy --all-targets -- -D warnings
echo '[4/5] Unit tests'
cargo test --all-targets
echo '[5/5] Release compilation'
cargo build --release
printf '\nBuilt: target/release/mp4rust-native\n'
./target/release/mp4rust-native --check
