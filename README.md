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

**systems scope** (`src/systems.rs`, redis-backed) — a maintenance/chore tracker
with a **synthetic forcing function**: a non-terminating "keep doing X" goal is
turned into an accruing *deficit* that pages you via ntfy, so a skipped habit has
a real deadline instead of silently rotting.

Every track has a `kind` fixing how a logged session's `load` is computed:

- `box` — `{ duration_min, intensity }`, `load = duration_min * intensity`
  (high-variance, e.g. a workout).
- `check` — `{}`, `load = unit_load` (binary "did it").
- `loe` — `{ loe: "low"|"med"|"high" }`, `load = unit_load * {1,2,3}`.

Deficit/fitness is **never stored** — it's always recomputed from the raw session
log (EMA fitness vs a maintenance floor), so the model is restart-safe and
retunable. All config (tracks, tunables, schedules, ntfy settings) lives in redis
and is driven by the UI; this scope reads **no env beyond `REDIS_HOST`/`REDIS_PORT`**.

Endpoints (all under `/api/systems`, JSON):

- `GET|PUT /api/systems/settings` — global settings (ntfy URL, dashboard URL,
  `alert_hour`, `warmup_days`); GET returns defaults if unset.
- `GET /api/systems/overview` — one summary row per track.
- `GET|POST /api/systems/tracks` — list / create (server slugifies `name`->`id`,
  assigns `created_ms`, 409 if the id exists).
- `GET|PUT|DELETE /api/systems/tracks/:id` — fetch / update mutable fields (kind
  immutable) / delete (also drops its sessions/skips/scheduled/last_alert).
- `POST /api/systems/tracks/:id/session` — log a session (kind-specific body) ->
  `{ load }`.
- `POST /api/systems/tracks/:id/skip` — log a skip `{ scheduled_date, reason? }`;
  a null/empty reason is an *unlogged* skip (the relapse signal).
- `GET /api/systems/tracks/:id/dashboard` — full per-track model (fitness +
  daily-load series, floor, deficit, 14-day adherence).

A detached background task ticks every 15 min: past `alert_hour` it materializes
each track's due obligation, recomputes the deficit, and pages ntfy once per day
per track when `deficit_load > alert_threshold` OR there are unlogged skips.
Empty `ntfy_url` disables paging.

Redis keys (namespace `systems:`): `systems:settings` (JSON), `systems:tracks`
(hash `id -> track JSON`), `systems:sessions:<id>` / `systems:skips:<id>` (lists),
`systems:scheduled:<id>` (hash `date -> {materialized_ms}`), `systems:last_alert:<id>`
(string `YYYY-MM-DD`, same-day page dedupe).

Routes are registered in `src/server.rs` (`setup_countdowns` + `setup_mc` +
`setup_systems`).

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
