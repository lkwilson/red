# Pinned, not `rust:alpine`. A rolling base image is the sccache poison vector:
# sccache fingerprints the compiler via `rustc -vV`, which stays identical when
# Alpine rebuilds the *same* rustc version against a newer musl. Cached objects
# built against the old std then get linked into a build using the new one ->
# ABI mismatch -> SIGSEGV on boot. Bump this tag (Renovate can) to move rustc.
FROM rust:1.96-alpine3.24 AS builder
WORKDIR /app
COPY Cargo.lock Cargo.lock
COPY Cargo.toml Cargo.toml
COPY src src

# Use ARG to get the target platform
ARG TARGETPLATFORM

# Shared compiler cache (self-hosted MinIO, see scripts/check.sh). Non-secret
# so it's passed via --build-arg; falls back to a local disk cache if unset.
ARG SCCACHE_VERSION=0.8.2
ARG SCCACHE_BUCKET
ARG SCCACHE_ENDPOINT
ARG SCCACHE_REGION
ARG SCCACHE_S3_USE_SSL
ARG SCCACHE_S3_KEY_PREFIX
ENV RUSTC_WRAPPER=sccache

# AWS creds are secret, so they're mounted rather than passed as build args
# (build args end up in the image history/cache).
RUN --mount=type=secret,id=sccache_aws_access_key_id \
    --mount=type=secret,id=sccache_aws_secret_access_key \
    apk add --no-cache musl-dev curl && \
    case "$TARGETPLATFORM" in \
        "linux/amd64") TARGET="x86_64-unknown-linux-musl"; SCCACHE_ARCH="x86_64" ;; \
        "linux/arm64") TARGET="aarch64-unknown-linux-musl"; SCCACHE_ARCH="aarch64" ;; \
        *) echo "Unsupported platform: $TARGETPLATFORM" && exit 1 ;; \
    esac && \
    curl -L "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-${SCCACHE_ARCH}-unknown-linux-musl.tar.gz" \
      | tar xz -C /tmp && \
    install -m 755 "/tmp/sccache-v${SCCACHE_VERSION}-${SCCACHE_ARCH}-unknown-linux-musl/sccache" /usr/local/bin/sccache && \
    rustup target add "$TARGET" && \
    export AWS_ACCESS_KEY_ID="$(cat /run/secrets/sccache_aws_access_key_id 2>/dev/null || true)" && \
    export AWS_SECRET_ACCESS_KEY="$(cat /run/secrets/sccache_aws_secret_access_key 2>/dev/null || true)" && \
    [ -n "$SCCACHE_BUCKET" ] || unset SCCACHE_BUCKET; \
    [ -n "$SCCACHE_ENDPOINT" ] || unset SCCACHE_ENDPOINT; \
    [ -n "$SCCACHE_REGION" ] || unset SCCACHE_REGION; \
    [ -n "$SCCACHE_S3_USE_SSL" ] || unset SCCACHE_S3_USE_SSL; \
    # Scope the sccache namespace by target triple + exact rustc version, so a
    # toolchain/base-image bump lands in a *fresh* namespace instead of reusing
    # objects from a different std/musl, and so release/arch/dev(check) builds
    # can never hand each other an object. This is the real guard against the
    # poisoning above; the pin just keeps the version string honest.
    if [ -n "$SCCACHE_S3_KEY_PREFIX" ]; then \
        export SCCACHE_S3_KEY_PREFIX="${SCCACHE_S3_KEY_PREFIX}/${TARGET}/rustc-$(rustc --version | awk '{print $2}')"; \
    else unset SCCACHE_S3_KEY_PREFIX; fi; \
    [ -n "$AWS_ACCESS_KEY_ID" ] || unset AWS_ACCESS_KEY_ID; \
    [ -n "$AWS_SECRET_ACCESS_KEY" ] || unset AWS_SECRET_ACCESS_KEY; \
    cargo build --release --target "$TARGET" --jobs 6 && \
    sccache --show-stats || true && \
    mkdir -p /app/target/release && \
    cp "/app/target/$TARGET/release/red" /app/target/release/red

FROM alpine:3.24
WORKDIR /app
COPY --from=builder /app/target/release/red bin/red
RUN apk add --no-cache libgcc && \
    chmod +x bin/red
ENV PATH="/app/bin:${PATH}"
VOLUME /app/config.yaml
CMD ["red"]
