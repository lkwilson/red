#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/rust.sh
cargo clippy --locked --workspace --all-targets --all-features -- \
  -W clippy::unwrap_used \
  -W clippy::expect_used \
  -W clippy::panic \
  -W clippy::unimplemented \
  -W clippy::todo \
  -D warnings
