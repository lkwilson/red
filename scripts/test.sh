#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/rust.sh
# Cargo builds the dev-profile test executables before running them.
cargo test --locked --workspace --all-features --profile dev
"$RUSTC_WRAPPER" --show-stats
