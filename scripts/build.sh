#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/rust.sh
# Test dependencies/artifacts remain in this same native target and dev profile.
# Build the normal application executables and any remaining development targets.
cargo build --locked --workspace --all-targets --all-features --profile dev
"$RUSTC_WRAPPER" --show-stats
