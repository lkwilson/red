# red

The countdown backend service (a Rust/axum API), split out of `hub`.

Follows the standard `home-infra` pipeline (see `home-infra/templates`), but
images are **amd64-only**: every cluster node is x86, so CI pushes a single-arch
image straight to `<tag>` (no `<tag>-amd64` + manifest step). Dev happens on arm
Macs; only the build target is x86.

## Endpoints

- `GET /` — liveness banner.
- `GET /health` — health probe.

Routes are registered in `src/server.rs`; add the countdown API here.

## Checks

```
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- \
  -W clippy::unwrap_used \
  -W clippy::expect_used \
  -W clippy::panic \
  -W clippy::unimplemented \
  -W clippy::todo \
  -D warnings
cargo test --verbose
```

