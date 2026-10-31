#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
RUSTDOCFLAGS="-D missing_docs -D rustdoc::broken_intra_doc_links" cargo doc --workspace --all-features --no-deps
cargo build --release --target thumbv7em-none-eabihf -p flightbag-core -p flightbag
cargo build --release --target wasm32-unknown-unknown -p flightbag-core -p flightbag
echo "==> Flightbag CI passed"
