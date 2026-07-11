//! Minecraft control-panel endpoints — the `/api/mc/*` scope, consumed by
//! `mc-ui`. Lets an operator, per server: list the running pods, tail the live
//! server log, and drive an interactive RCON console.
//!
//! red runs as an in-cluster pod, so it reaches the k8s API via its mounted
//! ServiceAccount (`red`, least-privilege — see home-infra
//! `clusters/brew7/apps/configs/red-mc-rbac.yaml`) and reaches each server's
//! RCON over the in-cluster `mc-<name>-rcon` ClusterIP Service. The k8s client
//! pattern mirrors `hub/src/ops.rs`; `Config::infer()` falls back to the local
//! kubeconfig for `cargo run`, and if no client can be built the endpoints
//! report `503` rather than sinking the server (countdown keeps working).
//!
//! Config (env, injected by the Deployment; sane defaults match the mc chart):
//!   MC_NAMESPACE (mc), RCON_PORT (25575), RCON_PASSWORD (minecraft).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use futures::{AsyncBufReadExt, StreamExt};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, LogParams};
use kube::{Client, Config};
use rcon::Connection;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tracing::warn;

/// kube-rs defaults connect/read/write timeouts to 295s (kube-rs#146). We cap
/// connect+write so a hung apiserver can't wedge a request past shutdown (same
/// rationale as hub/src/ops.rs), but leave read_timeout OFF: `log_stream` with
/// follow holds the response open indefinitely, and a quiet server can go many
/// minutes without a line — an idle-read timeout would sever the log tail.
const K8S_REQ_TIMEOUT: Duration = Duration::from_secs(8);

/// Keep quiet log WebSockets alive through reverse proxies and load balancers.
/// This is a WebSocket control frame, not a log line, so browsers answer with a
/// Pong automatically and mc-ui never renders it.
const LOG_WS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);

/// Bound RCON connect + each command so a wedged server can't hang a socket.
const RCON_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap the Loki HTTP call so a wedged Loki can't hang a history request past
/// shutdown (same spirit as `K8S_REQ_TIMEOUT`).
const LOKI_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard cap on lines requested from Loki in one `query_range` (boots * lines is
/// clamped to this), so an absurd `?boots=&lines=` can't ask Loki for the moon.
const LOKI_LIMIT_CAP: usize = 5000;

/// The itzg container name. Backup-enabled servers run a second container
/// (`backup`), so `log_stream` must name the container or the apiserver rejects
/// the request as ambiguous.
const MC_CONTAINER: &str = "minecraft-server";

struct McState {
    /// None if no k8s client could be built (e.g. `cargo run` with no
    /// kubeconfig); handlers then report unavailable instead of failing.
    client: Option<Client>,
    namespace: String,
    rcon_port: String,
    rcon_password: String,
    /// Base URL of in-cluster Loki (ns `monitoring`); cross-namespace DNS makes
    /// the default correct in prod with no Deployment change.
    loki_url: String,
    /// Shared HTTP client for Loki, timeout-capped once (see `LOKI_TIMEOUT`).
    http: reqwest::Client,
}

/// One row of the mc-ui server list.
#[derive(Serialize)]
struct ServerInfo {
    /// Server name without the `mc-` deployment prefix (e.g. `bolty`).
    name: String,
    /// True when scaled to >= 1 replica.
    enabled: bool,
    /// Desired replicas.
    replicas: i32,
    /// Ready replicas.
    ready: i32,
    /// Running pod name, if any.
    pod: Option<String>,
    /// Pod phase (Running / Pending / ...), if a pod exists.
    phase: Option<String>,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Only allow names that are safe to splice into a label selector and a DNS
/// name (`mc-<name>-rcon.<ns>.svc...`). Real server names are simple.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn unavailable() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "k8s client unavailable").into_response()
}

fn bad_gateway(err: &anyhow::Error) -> Response {
    warn!("mc: upstream error: {err:?}");
    (StatusCode::BAD_GATEWAY, "upstream error").into_response()
}

/// Same inference `Client::try_default()` uses, but connect/write are capped and
/// read is left uncapped for streaming (see `K8S_REQ_TIMEOUT`).
async fn build_client() -> Result<Client> {
    let mut config = Config::infer().await?;
    config.connect_timeout = Some(K8S_REQ_TIMEOUT);
    config.write_timeout = Some(K8S_REQ_TIMEOUT);
    config.read_timeout = None;
    Ok(Client::try_from(config)?)
}

/// List every mc server from its Deployment (so scaled-to-0 servers still show),
/// joined with its running pod for name + phase.
async fn list_servers(client: &Client, ns: &str) -> Result<Vec<ServerInfo>> {
    let deploys: Api<Deployment> = Api::namespaced(client.clone(), ns);
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let dlist = deploys.list(&ListParams::default()).await?;
    let plist = pods.list(&ListParams::default()).await?;

    // Index pods by their `app` label (== the deployment name, `mc-<name>`).
    let mut pod_by_app: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
    for p in plist {
        let app = p
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("app").cloned());
        if let (Some(app), Some(name)) = (app, p.metadata.name.clone()) {
            let phase = p.status.as_ref().and_then(|s| s.phase.clone());
            pod_by_app.insert(app, (name, phase));
        }
    }

    let mut out = Vec::new();
    for d in dlist {
        let Some(dname) = d.metadata.name.clone() else {
            continue;
        };
        let Some(name) = dname.strip_prefix("mc-").map(|s| s.to_string()) else {
            continue;
        };
        let replicas = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0);
        let ready = d
            .status
            .as_ref()
            .and_then(|s| s.ready_replicas)
            .unwrap_or(0);
        let (pod, phase) = match pod_by_app.get(&dname) {
            Some((p, ph)) => (Some(p.clone()), ph.clone()),
            None => (None, None),
        };
        out.push(ServerInfo {
            name,
            enabled: replicas > 0,
            replicas,
            ready,
            pod,
            phase,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// GET /api/mc/servers -> `[ServerInfo]`.
async fn handle_servers(State(state): State<Arc<McState>>) -> Response {
    let Some(client) = state.client.clone() else {
        return unavailable();
    };
    match list_servers(&client, &state.namespace).await {
        Ok(servers) => Json(servers).into_response(),
        Err(err) => bad_gateway(&err),
    }
}

/// GET /api/mc/servers/:name/logs -> WebSocket streaming the live server log.
async fn logs_ws(
    ws: WebSocketUpgrade,
    Path(name): Path<String>,
    State(state): State<Arc<McState>>,
) -> Response {
    if !valid_name(&name) {
        return (StatusCode::BAD_REQUEST, "invalid server name").into_response();
    }
    let Some(client) = state.client.clone() else {
        return unavailable();
    };
    let ns = state.namespace.clone();
    ws.on_upgrade(move |socket| stream_logs(socket, client, ns, name))
}

async fn stream_logs(mut socket: WebSocket, client: Client, ns: String, name: String) {
    let pods: Api<Pod> = Api::namespaced(client, &ns);
    let selector = format!("app=mc-{name}");
    let pod_name = match pods.list(&ListParams::default().labels(&selector)).await {
        Ok(list) => list.into_iter().find_map(|p| p.metadata.name),
        Err(err) => {
            let _ = socket.send(Message::Text(format!("error: {err}"))).await;
            return;
        }
    };
    let Some(pod_name) = pod_name else {
        let _ = socket
            .send(Message::Text(format!("no running pod for {name}")))
            .await;
        return;
    };

    let params = LogParams {
        follow: true,
        tail_lines: Some(500),
        container: Some(MC_CONTAINER.to_string()),
        ..Default::default()
    };
    let reader = match pods.log_stream(&pod_name, &params).await {
        Ok(r) => r,
        Err(err) => {
            let _ = socket.send(Message::Text(format!("error: {err}"))).await;
            return;
        }
    };
    // futures-io AsyncBufRead -> a Stream of lines; pin so it's pollable in select!.
    let lines = reader.lines();
    futures::pin_mut!(lines);
    let mut heartbeats = tokio::time::interval(LOG_WS_HEARTBEAT_INTERVAL);
    heartbeats.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` ticks immediately on creation; consume that tick so the first
    // heartbeat is sent only after a full quiet interval.
    heartbeats.tick().await;

    loop {
        tokio::select! {
            line = lines.next() => match line {
                Some(Ok(l)) => {
                    if socket.send(Message::Text(l)).await.is_err() {
                        break;
                    }
                }
                Some(Err(err)) => {
                    let _ = socket.send(Message::Text(format!("stream error: {err}"))).await;
                    break;
                }
                None => break,
            },
            // Watch for the client closing so the log_stream is dropped promptly.
            msg = socket.recv() => match msg {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                _ => {}
            },
            _ = heartbeats.tick() => {
                if socket.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
            },
        }
    }
}

/// GET /api/mc/servers/:name/rcon -> bidirectional WebSocket RCON console. Each
/// inbound text frame is a command; each reply frame is the RCON response.
async fn rcon_ws(
    ws: WebSocketUpgrade,
    Path(name): Path<String>,
    State(state): State<Arc<McState>>,
) -> Response {
    if !valid_name(&name) {
        return (StatusCode::BAD_REQUEST, "invalid server name").into_response();
    }
    let addr = format!(
        "mc-{name}-rcon.{}.svc.cluster.local:{}",
        state.namespace, state.rcon_port
    );
    let password = state.rcon_password.clone();
    ws.on_upgrade(move |socket| handle_rcon(socket, addr, password))
}

async fn handle_rcon(mut socket: WebSocket, addr: String, password: String) {
    let connect = <Connection<TcpStream>>::builder()
        .enable_minecraft_quirks(true)
        .connect(addr.as_str(), &password);
    let mut conn = match tokio::time::timeout(RCON_TIMEOUT, connect).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(err)) => {
            let _ = socket
                .send(Message::Text(format!("rcon connect failed: {err}")))
                .await;
            return;
        }
        Err(_) => {
            let _ = socket
                .send(Message::Text(format!("rcon connect timed out ({addr})")))
                .await;
            return;
        }
    };
    let _ = socket
        .send(Message::Text(format!("connected to {addr}")))
        .await;

    while let Some(msg) = socket.recv().await {
        let cmd = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        let cmd = cmd.trim();
        if cmd.is_empty() {
            continue;
        }
        let reply = match tokio::time::timeout(RCON_TIMEOUT, conn.cmd(cmd)).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(err)) => format!("rcon error: {err}"),
            Err(_) => "rcon command timed out".to_string(),
        };
        if socket.send(Message::Text(reply)).await.is_err() {
            break;
        }
    }
}

/// `GET /api/mc/servers/:name/history` query params (all optional).
#[derive(Deserialize)]
struct HistoryParams {
    /// Max boots to return (newest first).
    #[serde(default = "default_boots")]
    boots: usize,
    /// Max tail lines kept per boot.
    #[serde(default = "default_lines")]
    lines: usize,
    /// How far back to query Loki, in hours.
    #[serde(default = "default_lookback_hours")]
    lookback_hours: u64,
}

fn default_boots() -> usize {
    10
}
fn default_lines() -> usize {
    500
}
fn default_lookback_hours() -> u64 {
    720
}

/// One container boot's log tail. camelCase to match the mc-ui contract.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Boot {
    /// Pod the boot ran in (`stream.pod`).
    pod: String,
    /// Stable per-boot id: `<pod>#<restartCount>`, or the raw filename if the
    /// restart count can't be parsed.
    id: String,
    /// Min entry timestamp of the returned lines, Unix ms.
    start_ms: u64,
    /// Max entry timestamp of the returned lines, Unix ms.
    end_ms: u64,
    /// The boot's tail: chronological ascending, most-recent `lines` at the end.
    lines: Vec<String>,
}

#[derive(Serialize)]
struct HistoryResponse {
    boots: Vec<Boot>,
}

// --- Loki `query_range` response shape (only the fields we read). ---

#[derive(Deserialize)]
struct LokiResponse {
    data: LokiData,
}
#[derive(Deserialize)]
struct LokiData {
    #[serde(default)]
    result: Vec<LokiStream>,
}
#[derive(Deserialize)]
struct LokiStream {
    #[serde(default)]
    stream: LokiLabels,
    /// Each value is `["<ts_ns_string>", "<line>"]` (a trailing structured-
    /// metadata object is tolerated by reading only indices 0 and 1).
    #[serde(default)]
    values: Vec<Vec<serde_json::Value>>,
}
#[derive(Deserialize, Default)]
struct LokiLabels {
    #[serde(default)]
    pod: String,
    #[serde(default)]
    filename: String,
}

/// Accumulates one boot's entries while grouping streams by filename.
struct BootAccum {
    pod: String,
    filename: String,
    entries: Vec<(u64, String)>,
}

/// `<pod>#<restartCount>` parsed from the `filename` label
/// (`.../minecraft-server/<restartCount>.log`); falls back to the raw filename.
fn boot_id(pod: &str, filename: &str) -> String {
    filename
        .rsplit('/')
        .next()
        .and_then(|f| f.strip_suffix(".log"))
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .map(|n| format!("{pod}#{n}"))
        .unwrap_or_else(|| filename.to_string())
}

/// Pure boot-grouping/sort/tail over a raw Loki `query_range` body. Factored out
/// so it's unit-testable without a live Loki. Each distinct `filename` is one
/// container boot; streams sharing a filename are defensively merged.
fn boots_from_loki(body: &str, max_boots: usize, max_lines: usize) -> Result<Vec<Boot>> {
    let parsed: LokiResponse = serde_json::from_str(body)?;

    let mut groups: BTreeMap<String, BootAccum> = BTreeMap::new();
    for stream in parsed.data.result {
        let pod = stream.stream.pod;
        let filename = stream.stream.filename;
        let acc = groups.entry(filename.clone()).or_insert_with(|| BootAccum {
            pod: pod.clone(),
            filename,
            entries: Vec::new(),
        });
        if acc.pod.is_empty() && !pod.is_empty() {
            acc.pod = pod;
        }
        for v in stream.values {
            let (Some(ts), Some(line)) = (
                v.first().and_then(serde_json::Value::as_str),
                v.get(1).and_then(serde_json::Value::as_str),
            ) else {
                continue;
            };
            let Ok(ts_ns) = ts.parse::<u64>() else {
                continue;
            };
            acc.entries.push((ts_ns, line.to_string()));
        }
    }

    let mut boots: Vec<Boot> = groups
        .into_values()
        .filter_map(|mut acc| {
            acc.entries.sort_by_key(|(ts, _)| *ts);
            if acc.entries.len() > max_lines {
                let drop_to = acc.entries.len() - max_lines;
                acc.entries.drain(0..drop_to);
            }
            let start_ms = acc.entries.first().map(|(ts, _)| ts / 1_000_000)?;
            let end_ms = acc.entries.last().map(|(ts, _)| ts / 1_000_000)?;
            let id = boot_id(&acc.pod, &acc.filename);
            let lines = acc.entries.into_iter().map(|(_, l)| l).collect();
            Some(Boot {
                pod: acc.pod,
                id,
                start_ms,
                end_ms,
                lines,
            })
        })
        .collect();

    boots.sort_by_key(|b| std::cmp::Reverse(b.end_ms));
    boots.truncate(max_boots);
    Ok(boots)
}

/// Query Loki for the server's boot history. This path only needs Loki (not the
/// k8s client), so it deliberately does NOT gate on `state.client`/`unavailable`
/// — a scaled-to-0 server with no pod still has retained log history.
async fn fetch_history(state: &McState, name: &str, params: &HistoryParams) -> Result<Vec<Boot>> {
    let now_ns: u64 = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u64;
    let lookback_ns = params.lookback_hours.saturating_mul(3_600_000_000_000);
    let start_ns = now_ns.saturating_sub(lookback_ns);
    let limit = params
        .boots
        .saturating_mul(params.lines)
        .min(LOKI_LIMIT_CAP);

    let query = format!(
        r#"{{namespace="{}", app="mc-{}", container="{}"}}"#,
        state.namespace, name, MC_CONTAINER
    );
    let url = format!("{}/loki/api/v1/query_range", state.loki_url);
    // Loki is single-tenant (auth_enabled=false), so no X-Scope-OrgID header.
    let resp = state
        .http
        .get(&url)
        .query(&[
            ("query", query.as_str()),
            ("start", &start_ns.to_string()),
            ("end", &now_ns.to_string()),
            ("direction", "backward"),
            ("limit", &limit.to_string()),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("loki query_range {status}: {body}");
    }
    let text = resp.text().await?;
    boots_from_loki(&text, params.boots, params.lines)
}

/// GET /api/mc/servers/:name/history -> `HistoryResponse` of previous-boot logs
/// from Loki. 400 on invalid name; 502 if Loki is unreachable / errors / body
/// doesn't parse.
async fn history(
    Path(name): Path<String>,
    Query(params): Query<HistoryParams>,
    State(state): State<Arc<McState>>,
) -> Response {
    if !valid_name(&name) {
        return (StatusCode::BAD_REQUEST, "invalid server name").into_response();
    }
    match fetch_history(&state, &name, &params).await {
        Ok(boots) => Json(HistoryResponse { boots }).into_response(),
        Err(err) => bad_gateway(&err),
    }
}

/// Registers the `/api/mc/*` scope. Builds the k8s client once; a failure here
/// doesn't sink the server — the endpoints just report unavailable.
pub async fn setup_mc(app: Router) -> Result<Router> {
    let client = match build_client().await {
        Ok(c) => Some(c),
        Err(err) => {
            warn!("mc: no k8s client ({err}); /api/mc/* will report unavailable");
            None
        }
    };
    // Shared, timeout-capped HTTP client for Loki. Build failure is effectively
    // never (no network I/O here); propagate it rather than silently degrade.
    let http = reqwest::Client::builder().timeout(LOKI_TIMEOUT).build()?;
    let state = Arc::new(McState {
        client,
        namespace: env_or("MC_NAMESPACE", "mc"),
        rcon_port: env_or("RCON_PORT", "25575"),
        rcon_password: env_or("RCON_PASSWORD", "minecraft"),
        loki_url: env_or("LOKI_URL", "http://loki.monitoring.svc.cluster.local:3100"),
        http,
    });

    let mc = Router::new()
        .route("/api/mc/servers", get(handle_servers))
        .route("/api/mc/servers/:name/logs", get(logs_ws))
        .route("/api/mc/servers/:name/rcon", get(rcon_ws))
        .route("/api/mc/servers/:name/history", get(history))
        .with_state(state);

    Ok(app.merge(mc))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAYLOAD: &str = r#"{
      "data": {
        "resultType": "streams",
        "result": [
          {
            "stream": {
              "pod": "mc-bolty-59bd4dcd55-l4cxd",
              "filename": "/var/log/pods/mc_mc-bolty-59bd4dcd55-l4cxd_uid/minecraft-server/1.log"
            },
            "values": [
              ["1720730460000000000", "later line"],
              ["1720730100000000000", "earlier line"]
            ]
          },
          {
            "stream": {
              "pod": "mc-bolty-6c9f7d-abcde",
              "filename": "/var/log/pods/mc_mc-bolty-6c9f7d-abcde_uid/minecraft-server/0.log"
            },
            "values": [
              ["1720740000000000000", "boot2 a"],
              ["1720740100000000000", "boot2 b"],
              ["1720740200000000000", "boot2 c"]
            ]
          }
        ]
      }
    }"#;

    #[test]
    fn groups_by_boot_sorts_newest_first_and_tails() {
        let boots = boots_from_loki(PAYLOAD, 10, 2).unwrap_or_default();
        assert_eq!(boots.len(), 2);

        // Newest boot (higher end_ms) comes first.
        let newest = &boots[0];
        assert_eq!(newest.id, "mc-bolty-6c9f7d-abcde#0");
        assert_eq!(newest.pod, "mc-bolty-6c9f7d-abcde");
        // Tailed to the last 2 lines, ascending; "boot2 a" dropped.
        assert_eq!(newest.lines, vec!["boot2 b", "boot2 c"]);
        assert_eq!(newest.start_ms, 1_720_740_100_000);
        assert_eq!(newest.end_ms, 1_720_740_200_000);

        // Older boot: values sorted ascending, ns -> ms.
        let older = &boots[1];
        assert_eq!(older.id, "mc-bolty-59bd4dcd55-l4cxd#1");
        assert_eq!(older.lines, vec!["earlier line", "later line"]);
        assert_eq!(older.start_ms, 1_720_730_100_000);
        assert_eq!(older.end_ms, 1_720_730_460_000);
    }

    #[test]
    fn respects_max_boots() {
        let boots = boots_from_loki(PAYLOAD, 1, 500).unwrap_or_default();
        assert_eq!(boots.len(), 1);
        assert_eq!(boots[0].id, "mc-bolty-6c9f7d-abcde#0");
    }

    #[test]
    fn boot_id_falls_back_to_filename_when_unparseable() {
        assert_eq!(
            boot_id("p", "/var/log/pods/x/minecraft-server/3.log"),
            "p#3"
        );
        assert_eq!(boot_id("p", "weird-name"), "weird-name");
        assert_eq!(boot_id("p", "/a/b/notanumber.log"), "/a/b/notanumber.log");
    }

    #[test]
    fn empty_result_yields_no_boots() {
        let boots = boots_from_loki(r#"{"data":{"result":[]}}"#, 10, 500).unwrap_or_default();
        assert!(boots.is_empty());
    }
}
