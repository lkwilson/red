FROM rust:alpine AS builder
WORKDIR /app
COPY Cargo.lock Cargo.lock
COPY Cargo.toml Cargo.toml
COPY src src

# amd64-only: every cluster node is x86, so we skip the multi-arch dance and
# build a single musl target. Dev happens on arm Macs, but images are x86.
RUN apk add --no-cache musl-dev && \
    rustup target add x86_64-unknown-linux-musl && \
    cargo build --release --target x86_64-unknown-linux-musl --jobs 6 && \
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
