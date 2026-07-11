# red

A Rust/axum backend hosting two scoped APIs: the **countdown** store (split out
of `hub`) and the **mc** control panel (consumed by `mc-ui`).

Follows the standard `home-infra` pipeline (see `home-infra/templates`), but
images are **amd64-only**: every cluster node is x86, so CI pushes a single-arch
image straight to `<tag>` (no `<tag>-amd64` + manifest step). Dev happens on arm
Macs; only the build target is x86.

## Endpoints

- `GET /` — liveness banner.
- `GET /health` — health probe.

**Countdown scope** (`src/countdowns.rs`, redis-backed):

- `GET|POST /api/countdown/countdowns`, `GET|DELETE /api/countdown/countdowns/:name`.

**mc scope** (`src/mc.rs`, in-cluster k8s client + RCON) — the Minecraft control
panel behind `mc-ui`:

- `GET /api/mc/servers` — list mc servers (from their Deployments/pods in ns `mc`).
- `GET /api/mc/servers/:name/logs` — **WebSocket** streaming the live pod log
  (k8s `log_stream`, `minecraft-server` container).
- `GET /api/mc/servers/:name/rcon` — bidirectional **WebSocket** RCON console;
  each text frame is a command, each reply the response. Connects to the
  `mc-<name>-rcon` ClusterIP Service.
- `GET /api/mc/servers/:name/history` — previous-boot log history from Grafana
  Loki (ns `monitoring`, 90d retention), since k3s only keeps ~1 boot of
  container logs. Optional query params `?boots=<N>` (default 10),
  `?lines=<M>` (default 500), `?lookback_hours=<H>` (default 720 = 30d). Returns
  `{ "boots": [ { pod, id, startMs, endMs, lines } ] }`, newest boot first, each
  boot's `lines` a chronological tail. Only needs Loki (not the k8s client), so
  a scaled-to-0 server still returns history. Loki errors -> `502`.

The mc scope needs an in-cluster ServiceAccount with least-privilege access to ns
`mc` (pods + pods/log): `home/red`, RBAC in home-infra
`clusters/brew7/apps/configs/red-mc-rbac.yaml`. Running locally without a
kubeconfig, `/api/mc/*` reports `503` and countdown still works. Config is via
env (`MC_NAMESPACE`, `RCON_PORT`, `RCON_PASSWORD`; defaults match the mc chart).
The `history` endpoint reads Loki at `LOKI_URL` (default
`http://loki.monitoring.svc.cluster.local:3100`); in-cluster cross-namespace DNS
makes the default correct in prod with no Deployment change.

Routes are registered in `src/server.rs` (`setup_countdowns` + `setup_mc`).

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
