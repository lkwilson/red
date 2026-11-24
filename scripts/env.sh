#!/usr/bin/env bash
# Repo-specific settings. All other managed files must match the selected variant.
# No credentials, skip flags, or shell command strings here.
export APP_NAME=red
export APP_VARIANT=rust
IMAGE_PLATFORMS=(linux/amd64)
# CI supplies its Docker endpoint. Locally, use DOCKER_HOST/current context.
export IMAGE_DOCKER_ENDPOINT=${IMAGE_DOCKER_ENDPOINT:-}
# Empty for Rust-only apps; workflow selection of runs-on is fixed per variant.
export NODE_VERSION=""
export RUST_VERSION=1.100.0
export RUST_IMAGE=rust:1.100-alpine3.24
export XX_IMAGE=tonistiigi/xx:1.10.0
export SCCACHE_VERSION=0.20.1
export SCCACHE_BUCKET=sccache
export SCCACHE_ENDPOINT=https://minio.endsy.me
export SCCACHE_REGION=us-east-1
export SCCACHE_S3_USE_SSL=true
# Keep the host dev/test output shared across lint, tests and subsequent builds.
export CARGO_BUILD_JOBS=6
