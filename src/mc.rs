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
use std::time::Duration;

use anyhow::Result;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
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
use serde::Serialize;
use tokio::net::TcpStream;
use tracing::warn;

/// kube-rs defaults connect/read/write timeouts to 295s (kube-rs#146). We cap
/// connect+write so a hung apiserver can't wedge a request past shutdown (same
/// rationale as hub/src/ops.rs), but leave read_timeout OFF: `log_stream` with
/// follow holds the response open indefinitely, and a quiet server can go many
/// minutes without a line — an idle-read timeout would sever the log tail.
const K8S_REQ_TIMEOUT: Duration = Duration::from_secs(8);

/// Bound RCON connect + each command so a wedged server can't hang a socket.
const RCON_TIMEOUT: Duration = Duration::from_secs(10);

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
    let state = Arc::new(McState {
        client,
        namespace: env_or("MC_NAMESPACE", "mc"),
        rcon_port: env_or("RCON_PORT", "25575"),
        rcon_password: env_or("RCON_PASSWORD", "minecraft"),
    });

    let mc = Router::new()
        .route("/api/mc/servers", get(handle_servers))
        .route("/api/mc/servers/:name/logs", get(logs_ws))
        .route("/api/mc/servers/:name/rcon", get(rcon_ws))
        .with_state(state);

    Ok(app.merge(mc))
}
