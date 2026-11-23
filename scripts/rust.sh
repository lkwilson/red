#!/usr/bin/env bash
# Sourced by native Rust lint/test/build stages; no Docker or cross target.
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"
if [[ -x ${CARGO_HOME:-$HOME/.cargo}/bin/rustup ]]; then
  rustup_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
else
  rustup_path=$(command -v rustup)
  rustup_bin="$(dirname "$rustup_path")"
fi
export PATH="$rustup_bin:$PATH"
export RUSTUP_TOOLCHAIN=$RUST_VERSION
export RUSTC_WRAPPER="${XDG_CACHE_HOME:-$HOME/.cache}/app-ci/sccache-$SCCACHE_VERSION/sccache"
: "${AWS_ACCESS_KEY_ID:?sccache credentials required}" "${AWS_SECRET_ACCESS_KEY:?sccache credentials required}"
# Always use this machine's native toolchain target, including on ARM dev hosts.
export CARGO_BUILD_TARGET="$(rustc -vV | sed -n 's/^host: //p')"
export SCCACHE_S3_KEY_PREFIX="$APP_NAME/dev/$CARGO_BUILD_TARGET/rustc-$(rustc --version | awk '{print $2}')"
# A daemon keeps its startup cache configuration even when client env changes.
# Isolate concurrent repos/toolchains without stopping another build's daemon.
sccache_server_id=$(printf '%s\n' "$SCCACHE_VERSION" "$SCCACHE_ENDPOINT" "$SCCACHE_BUCKET" "$SCCACHE_REGION" "$SCCACHE_S3_USE_SSL" "$SCCACHE_S3_KEY_PREFIX" | sha256sum | cut -c 1-24)
export SCCACHE_SERVER_UDS="/tmp/app-ci-sccache-$UID-$sccache_server_id.sock"
