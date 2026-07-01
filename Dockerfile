FROM rust:alpine AS builder
WORKDIR /app
COPY Cargo.lock Cargo.lock
COPY Cargo.toml Cargo.toml
COPY src src

# amd64-only: every cluster node is x86, so we skip the multi-arch dance and
# build a single musl target. Dev happens on arm Macs, but images are x86.

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
    curl -L "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
      | tar xz -C /tmp && \
    install -m 755 "/tmp/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl/sccache" /usr/local/bin/sccache && \
    rustup target add x86_64-unknown-linux-musl && \
    export AWS_ACCESS_KEY_ID="$(cat /run/secrets/sccache_aws_access_key_id 2>/dev/null || true)" && \
    export AWS_SECRET_ACCESS_KEY="$(cat /run/secrets/sccache_aws_secret_access_key 2>/dev/null || true)" && \
    [ -n "$SCCACHE_BUCKET" ] || unset SCCACHE_BUCKET; \
    [ -n "$SCCACHE_ENDPOINT" ] || unset SCCACHE_ENDPOINT; \
    [ -n "$SCCACHE_REGION" ] || unset SCCACHE_REGION; \
    [ -n "$SCCACHE_S3_USE_SSL" ] || unset SCCACHE_S3_USE_SSL; \
    [ -n "$SCCACHE_S3_KEY_PREFIX" ] || unset SCCACHE_S3_KEY_PREFIX; \
    [ -n "$AWS_ACCESS_KEY_ID" ] || unset AWS_ACCESS_KEY_ID; \
    [ -n "$AWS_SECRET_ACCESS_KEY" ] || unset AWS_SECRET_ACCESS_KEY; \
    cargo build --release --target x86_64-unknown-linux-musl --jobs 6 && \
    sccache --show-stats || true && \
    mkdir -p /app/target/release && \
    cp /app/target/x86_64-unknown-linux-musl/release/red /app/target/release/red

FROM alpine:latest
WORKDIR /app
COPY --from=builder /app/target/release/red bin/red
RUN apk add --no-cache libgcc && \
    chmod +x bin/red
ENV PATH="/app/bin:${PATH}"
VOLUME /app/config.yaml
CMD ["red"]
