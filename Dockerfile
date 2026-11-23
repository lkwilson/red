ARG XX_IMAGE
ARG RUST_IMAGE
FROM --platform=$BUILDPLATFORM ${XX_IMAGE} AS xx
FROM --platform=$BUILDPLATFORM ${RUST_IMAGE} AS builder
COPY --from=xx / /
WORKDIR /app
RUN apk add --no-cache curl clang lld llvm file
ARG BUILDARCH
ARG TARGETPLATFORM
RUN xx-apk add --no-cache gcc musl-dev
ARG SCCACHE_VERSION
RUN set -eu; \
    case "$BUILDARCH" in amd64) arch=x86_64 ;; arm64) arch=aarch64 ;; *) exit 1 ;; esac; \
    curl -fL "https://github.com/mozilla/sccache/releases/download/v$SCCACHE_VERSION/sccache-v$SCCACHE_VERSION-$arch-unknown-linux-musl.tar.gz" -o /tmp/sccache.tar.gz; \
    tar xzf /tmp/sccache.tar.gz -C /tmp; \
    cp "/tmp/sccache-v$SCCACHE_VERSION-$arch-unknown-linux-musl/sccache" /usr/local/bin/sccache
# Include build.rs, Cargo configuration and app-owned compilation assets.
COPY . .
ARG APP_NAME
ARG RUST_IMAGE
ARG XX_IMAGE
ARG CARGO_BUILD_JOBS
ARG SCCACHE_BUCKET
ARG SCCACHE_ENDPOINT
ARG SCCACHE_REGION
ARG SCCACHE_S3_USE_SSL
ARG SCCACHE_S3_KEY_PREFIX
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=secret,id=sccache_aws_access_key_id,required=true \
    --mount=type=secret,id=sccache_aws_secret_access_key,required=true \
    set -eu; \
    target="$(xx-cargo --print-target-triple)"; \
    export RUSTC_WRAPPER=sccache; \
    export AWS_ACCESS_KEY_ID="$(cat /run/secrets/sccache_aws_access_key_id)"; \
    export AWS_SECRET_ACCESS_KEY="$(cat /run/secrets/sccache_aws_secret_access_key)"; \
    toolchain_id="$(printf '%s\n%s\n' "$RUST_IMAGE" "$XX_IMAGE" | sha256sum | cut -d ' ' -f 1)"; \
    export SCCACHE_S3_KEY_PREFIX="$SCCACHE_S3_KEY_PREFIX/release/cross-v1/$BUILDARCH/$target/toolchain-$toolchain_id/rustc-$(rustc --version | awk '{print $2}')"; \
    xx-cargo build --locked --profile release --bin "$APP_NAME" --jobs "$CARGO_BUILD_JOBS"; \
    sccache --show-stats; \
    xx-verify --static "/app/target/$target/release/$APP_NAME"; \
    mkdir /out; cp "/app/target/$target/release/$APP_NAME" /out/app
FROM alpine:3.24
WORKDIR /app
COPY --from=builder --chmod=755 /out/app /app/bin/app
CMD ["/app/bin/app"]
