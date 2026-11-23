#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/env.sh
# Prefer rustup proxies over cargo/rustc supplied by a surrounding Nix shell.
if [[ -x ${CARGO_HOME:-$HOME/.cargo}/bin/rustup ]]; then
  rustup_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
else
  rustup_path=$(command -v rustup)
  rustup_bin="$(dirname "$rustup_path")"
fi
export PATH="$rustup_bin:$PATH"
export RUSTUP_TOOLCHAIN=$RUST_VERSION
cargo fmt --all -- --check
