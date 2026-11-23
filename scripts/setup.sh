#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/env.sh
# Find existing proxies even when a Nix shell hides the user Cargo bin directory.
if [[ -x ${CARGO_HOME:-$HOME/.cargo}/bin/rustup ]]; then
  export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
fi
# Install rustup if needed; never select its rolling default toolchain.
if ! command -v rustup >/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
  export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
fi
# Linuxbrew and other rustup installations keep their proxies beside rustup.
rustup_bin="$(dirname "$(command -v rustup)")"
export PATH="$rustup_bin:$PATH"
if [[ -n ${GITHUB_PATH:-} ]]; then echo "$rustup_bin" >> "$GITHUB_PATH"; fi
rustup toolchain install "$RUST_VERSION" --profile minimal --component rustfmt --component clippy
sccache_dir="${XDG_CACHE_HOME:-$HOME/.cache}/app-ci/sccache-$SCCACHE_VERSION"
mkdir -p "$sccache_dir"
case $(uname -m) in x86_64) arch=x86_64 ;; aarch64) arch=aarch64 ;; *) exit 1 ;; esac
if [[ ! -x $sccache_dir/sccache ]]; then
  archive=$(mktemp)
  trap 'rm -f "$archive"' EXIT
  curl -fL "https://github.com/mozilla/sccache/releases/download/v$SCCACHE_VERSION/sccache-v$SCCACHE_VERSION-$arch-unknown-linux-musl.tar.gz" -o "$archive"
  tar xzf "$archive" --strip-components=1 -C "$sccache_dir"
fi
if [[ -n ${GITHUB_PATH:-} ]]; then echo "$sccache_dir" >> "$GITHUB_PATH"; fi
# Stage scripts select the pinned rustup proxies and absolute sccache path.
