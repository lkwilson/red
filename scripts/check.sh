#!/usr/bin/env bash
# Repo-specific checks (Rust).
set -euo pipefail

if ! command -v rustup >/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  echo "$HOME/.cargo/bin" >> "$GITHUB_PATH"
  export PATH="$HOME/.cargo/bin:$PATH"
fi
rustup component add rustfmt clippy

# Shared compiler cache (backed by self-hosted MinIO on brew7 -- see
# SCCACHE_ENDPOINT/README). Falls back to a local disk cache if SCCACHE_* env
# vars aren't set, so this is safe to always enable.
SCCACHE_VERSION="0.16.0"
if ! command -v sccache >/dev/null; then
  ARCH=$(uname -m)
  curl -L "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-${ARCH}-unknown-linux-musl.tar.gz" \
    | tar xz -C /tmp
  install -m 755 "/tmp/sccache-v${SCCACHE_VERSION}-${ARCH}-unknown-linux-musl/sccache" "$HOME/.cargo/bin/sccache"
fi
export RUSTC_WRAPPER=sccache

# Isolate this native (glibc host, test profile) build's cache namespace from
# the musl release-image build so they can never share an object. Scope by host
# target + rustc version; a rolling toolchain then starts a fresh, non-colliding
# namespace. (The release Dockerfile does the same by target.)
if [ -n "${SCCACHE_S3_KEY_PREFIX:-}" ]; then
  export SCCACHE_S3_KEY_PREFIX="${SCCACHE_S3_KEY_PREFIX}/check/$(rustc -vV | sed -n 's/^host: //p')/rustc-$(rustc --version | awk '{print $2}')"
fi

cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- \
  -W clippy::unwrap_used \
  -W clippy::expect_used \
  -W clippy::panic \
  -W clippy::unimplemented \
  -W clippy::todo \
  -D warnings
cargo test --verbose
sccache --show-stats || true
