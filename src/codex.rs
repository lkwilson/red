//! An on-demand availability probe and fixed-upstream HTTP bridge for cat7.
use anyhow::{bail, Context, Result};
use axum::{
    body::{boxed, Body, HttpBody},
    extract::{OriginalUri, State},
    http::{header, HeaderMap, Request, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{any, get},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::{env, time::Duration};

#[derive(Clone)]
struct Bridge {
    origin: String,
    client: reqwest::Client,
    probe_timeout: Duration,
    header_timeout: Duration,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Availability {
    status: &'static str,
    wrapper_running: bool,
    app_server_running: bool,
    workspace_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

impl Availability {
    fn inactive(reason: &'static str) -> Self {
        Self {
            status: "inactive",
            wrapper_running: false,
            app_server_running: false,
            workspace_root: None,
            reason: Some(reason),
        }
    }

    fn bad_response() -> Self {
        Self {
            status: "unavailable",
            wrapper_running: true,
            ..Self::inactive("bad_response")
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WrapperStatus {
    status: String,
    app_server_running: bool,
    workspace_root: String,
}

impl Bridge {
    fn new(origin: &str) -> Result<Self> {
        let url = reqwest::Url::parse(origin).context("invalid CODEX_WRAPPER_URL")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("CODEX_WRAPPER_URL must be an HTTP(S) origin without credentials, path or query");
        }
        Ok(Self {
            origin: url.as_str().trim_end_matches('/').to_owned(),
            // Ignore ambient HTTP_PROXY: this fixed LAN destination should be direct.
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(2))
                .build()?,
            probe_timeout: Duration::from_secs(2),
            header_timeout: Duration::from_secs(30),
        })
    }

    fn routes(self) -> Router {
        Router::new()
            .route("/api/codex/status", get(status).fallback(proxy))
            .route("/api/codex", any(proxy))
            .route("/api/codex/", any(proxy))
            .route("/api/codex/*path", any(proxy))
            .with_state(self)
    }
}

pub fn setup_codex(app: Router) -> Result<Router> {
    let origin = env::var("CODEX_WRAPPER_URL").unwrap_or_else(|_| "http://10.1.1.3:3000".into());
    Ok(app.merge(Bridge::new(&origin)?.routes()))
}

async fn status(State(bridge): State<Bridge>) -> impl IntoResponse {
    // This single deadline includes connecting, headers, body transfer and JSON parsing.
    let result = tokio::time::timeout(bridge.probe_timeout, async {
        let response = bridge
            .client
            .get(format!("{}/api/codex/status", bridge.origin))
            .send()
            .await;
        let mut response = match response {
            Ok(response) => response,
            Err(error) => {
                return Availability::inactive(if error.is_timeout() {
                    "timeout"
                } else {
                    "unreachable"
                })
            }
        };
        if !response.status().is_success() {
            return Availability::bad_response();
        }
        // Bound memory and synchronous parsing as well as wall time.
        let mut bytes = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) if bytes.len() + chunk.len() <= 16 * 1024 => {
                    bytes.extend_from_slice(&chunk)
                }
                Ok(Some(_)) | Err(_) => return Availability::bad_response(),
                Ok(None) => break,
            }
        }
        let Ok(value) = serde_json::from_slice::<WrapperStatus>(&bytes) else {
            return Availability::bad_response();
        };
        if !matches!(value.status.as_str(), "ready" | "unavailable")
            || (value.status == "ready") != value.app_server_running
        {
            return Availability::bad_response();
        }
        Availability {
            status: if value.app_server_running {
                "ready"
            } else {
                "unavailable"
            },
            wrapper_running: true,
            app_server_running: value.app_server_running,
            workspace_root: Some(value.workspace_root),
            reason: if value.app_server_running {
                None
            } else {
                Some("app_server_stopped")
            },
        }
    })
    .await;
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(result.unwrap_or_else(|_| Availability::inactive("timeout"))),
    )
}

/// Decode once for validation only; forward the original encoded URI unchanged.
fn safe_path(uri: &Uri) -> bool {
    let path = uri.path();
    if path != "/api/codex" && !path.starts_with("/api/codex/") {
        return false;
    }
    let mut decoded = Vec::new();
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let digits = [bytes.next(), bytes.next()];
            let [Some(a), Some(b)] = digits else {
                return false;
            };
            let Some(a) = char::from(a).to_digit(16) else {
                return false;
            };
            let Some(b) = char::from(b).to_digit(16) else {
                return false;
            };
            let decoded_byte = (a * 16 + b) as u8;
            // Block multiple decoding and encoded backslashes/control bytes.
            if decoded_byte == b'%'
                || decoded_byte == b'\\'
                || decoded_byte < 32
                || decoded_byte == 127
            {
                return false;
            }
            decoded.push(decoded_byte);
        } else {
            if byte == b'\\' {
                return false;
            }
            decoded.push(byte);
        }
    }
    !decoded
        .split(|byte| *byte == b'/')
        .any(|segment| segment == b"." || segment == b"..")
}

fn hop_headers(headers: &HeaderMap) -> Vec<String> {
    let mut names: Vec<String> = [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "host",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for value in headers.get_all(header::CONNECTION) {
        if let Ok(value) = value.to_str() {
            names.extend(
                value
                    .split(',')
                    .map(|name| name.trim().to_ascii_lowercase()),
            );
        }
    }
    names
}

async fn proxy(
    State(bridge): State<Bridge>,
    OriginalUri(uri): OriginalUri,
    request: Request<Body>,
) -> Response {
    if !safe_path(&uri) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid_path"})),
        )
            .into_response();
    }
    if request.headers().contains_key(header::UPGRADE) {
        return (StatusCode::NOT_IMPLEMENTED, Json(serde_json::json!({"error": "unsupported_upgrade", "message": "WebSocket proxying is not supported"}))).into_response();
    }
    let (parts, body) = request.into_parts();
    let excluded = hop_headers(&parts.headers);
    let mut upstream = bridge.client.request(
        match reqwest::Method::from_bytes(parts.method.as_str().as_bytes()) {
            Ok(method) => method,
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        },
        format!(
            "{}{}",
            bridge.origin,
            uri.path_and_query()
                .map_or(uri.path(), |value| value.as_str())
        ),
    );
    for (name, value) in &parts.headers {
        if !excluded.iter().any(|excluded| excluded == name.as_str()) {
            upstream = upstream.header(name.as_str(), value.as_bytes());
        }
    }
    let stream = futures::stream::unfold(body, |mut body| async {
        body.data().await.map(|chunk| (chunk, body))
    });
    let response = tokio::time::timeout(
        bridge.header_timeout,
        upstream.body(reqwest::Body::wrap_stream(stream)).send(),
    )
    .await;
    let response = match response {
        Ok(Ok(response)) => response,
        result => {
            let reason = match result {
                Err(_) => "timeout",
                Ok(Err(error)) if error.is_timeout() => "timeout",
                _ => "unreachable",
            };
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CACHE_CONTROL, "no-store")],
                Json(Availability::inactive(reason)),
            )
                .into_response();
        }
    };
    // reqwest uses http 1.x; axum 0.6 uses http 0.2. Copy wire values explicitly.
    let mut response_builder = axum::http::Response::builder().status(response.status().as_u16());
    let mut headers = HeaderMap::new();
    for (name, value) in response.headers() {
        if let (Ok(name), Ok(value)) = (
            header::HeaderName::from_bytes(name.as_str().as_bytes()),
            header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            headers.append(name, value);
        }
    }
    let excluded = hop_headers(&headers);
    for name in excluded {
        headers.remove(name);
    }
    if let Some(destination) = response_builder.headers_mut() {
        *destination = headers;
    }
    match response_builder.body(boxed(Body::wrap_stream(response.bytes_stream()))) {
        Ok(response) => response,
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use serde_json::{json, Value};

    struct Server(tokio::task::JoinHandle<()>);
    impl Drop for Server {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    fn serve(app: Router) -> Result<(String, Server)> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let origin = format!("http://{}", listener.local_addr()?);
        let server = axum::Server::from_tcp(listener)?.serve(app.into_make_service());
        Ok((
            origin,
            Server(tokio::spawn(async move {
                let _ = server.await;
            })),
        ))
    }

    fn frontend(origin: &str) -> Result<(String, Server)> {
        let mut bridge = Bridge::new(origin)?;
        bridge.probe_timeout = Duration::from_millis(80);
        bridge.header_timeout = Duration::from_millis(200);
        serve(bridge.routes().route("/health", get(|| async { "ok" })))
    }

    #[tokio::test]
    async fn status_distinguishes_ready_dead_and_invalid_wrapper() -> Result<()> {
        for (body, expected_status, app_running, reason) in [
            (
                json!({"status":"ready","appServerRunning":true,"workspaceRoot":"/home/op/w"}),
                "ready",
                true,
                None,
            ),
            (
                json!({"status":"unavailable","appServerRunning":false,"workspaceRoot":"/home/op/w"}),
                "unavailable",
                false,
                Some("app_server_stopped"),
            ),
            (
                json!({"status":"ready","appServerRunning":false,"workspaceRoot":"/home/op/w"}),
                "unavailable",
                false,
                Some("bad_response"),
            ),
        ] {
            let (upstream, _upstream) =
                serve(Router::new().route("/api/codex/status", get(move || async { Json(body) })))?;
            let (red, _red) = frontend(&upstream)?;
            let response = reqwest::get(format!("{red}/api/codex/status")).await?;
            assert_eq!(response.status(), 200);
            assert_eq!(response.headers()["cache-control"], "no-store");
            let value: Value = response.json().await?;
            assert_eq!(value["status"], expected_status);
            assert_eq!(value["wrapperRunning"], true);
            assert_eq!(value["appServerRunning"], app_running);
            assert_eq!(value.get("reason").and_then(Value::as_str), reason);
        }
        Ok(())
    }

    #[tokio::test]
    async fn refused_is_inactive_and_does_not_affect_red_health() -> Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let upstream = format!("http://{}", listener.local_addr()?);
        drop(listener);
        let (red, _red) = frontend(&upstream)?;
        let value: Value = reqwest::get(format!("{red}/api/codex/status"))
            .await?
            .json()
            .await?;
        assert_eq!(value["status"], "inactive");
        assert_eq!(value["wrapperRunning"], false);
        assert_eq!(value["reason"], "unreachable");
        let response = reqwest::get(format!("{red}/api/codex/projects")).await?;
        assert_eq!(response.status(), 503);
        assert_eq!(response.json::<Value>().await?["status"], "inactive");
        assert_eq!(
            reqwest::get(format!("{red}/health")).await?.text().await?,
            "ok"
        );
        Ok(())
    }

    #[tokio::test]
    async fn probe_deadline_includes_slow_body_and_headers() -> Result<()> {
        for slow_headers in [true, false] {
            let (upstream, _upstream) = serve(Router::new().route(
                "/api/codex/status",
                get(move || async move {
                    if slow_headers {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                    let (mut sender, body) = Body::channel();
                    tokio::spawn(async move {
                        let _ = sender.send_data(Vec::from(b"{".as_slice()).into()).await;
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        let _ = sender.send_data(Vec::from(b"}".as_slice()).into()).await;
                    });
                    axum::http::Response::new(body)
                }),
            ))?;
            let (red, _red) = frontend(&upstream)?;
            let started = tokio::time::Instant::now();
            let value: Value = reqwest::get(format!("{red}/api/codex/status"))
                .await?
                .json()
                .await?;
            assert_eq!(value["reason"], "timeout");
            assert_eq!(value["status"], "inactive");
            assert!(started.elapsed() < Duration::from_millis(250));
        }
        Ok(())
    }

    #[tokio::test]
    async fn proxy_preserves_method_uri_body_errors_and_end_to_end_headers() -> Result<()> {
        let (upstream, _upstream) = serve(Router::new().fallback(any(|request: Request<Body>| async {
            let (parts, mut body) = request.into_parts();
            let mut bytes = Vec::new();
            while let Some(chunk) = body.data().await { bytes.extend_from_slice(&chunk.map_err(|_| StatusCode::BAD_REQUEST)?); }
            assert!(parts.headers.get("x-remove").is_none());
            assert!(parts.headers.get("connection").is_none());
            assert_eq!(parts.headers["x-keep"], "yes");
            assert_ne!(parts.headers["host"], "wrong.example");
            Ok::<_, StatusCode>((StatusCode::IM_A_TEAPOT, [
                ("x-upstream", "kept"), ("connection", "x-secret"), ("x-secret", "removed"),
            ], Json(json!({"method":parts.method.as_str(), "uri":parts.uri.to_string(), "body":String::from_utf8_lossy(&bytes)}))))
        })))?;
        let (red, _red) = frontend(&upstream)?;
        let response = reqwest::Client::new()
            .patch(format!("{red}/api/codex/a%2Fb?q=a%2Fb&x=2"))
            .header("connection", "x-remove")
            .header("x-remove", "secret")
            .header("x-keep", "yes")
            .header("host", "wrong.example")
            .body("payload")
            .send()
            .await?;
        assert_eq!(response.status(), 418);
        assert_eq!(response.headers()["x-upstream"], "kept");
        assert!(response.headers().get("x-secret").is_none());
        assert!(response.headers().get("connection").is_none());
        let value: Value = response.json().await?;
        assert_eq!(
            value,
            json!({"method":"PATCH", "uri":"/api/codex/a%2Fb?q=a%2Fb&x=2", "body":"payload"})
        );
        Ok(())
    }

    #[tokio::test]
    async fn proxy_streams_without_probe_deadline_and_does_not_follow_redirects() -> Result<()> {
        let (upstream, _upstream) = serve(
            Router::new()
                .route(
                    "/api/codex/redirect",
                    get(|| async { (StatusCode::TEMPORARY_REDIRECT, [("location", "/outside")]) }),
                )
                .route("/outside", get(|| async { StatusCode::IM_A_TEAPOT }))
                .route(
                    "/api/codex/events",
                    get(|| async {
                        let (mut sender, body) = Body::channel();
                        tokio::spawn(async move {
                            let _ = sender
                                .send_data(Vec::from(b"data: one\n\n".as_slice()).into())
                                .await;
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            let _ = sender
                                .send_data(Vec::from(b"data: two\n\n".as_slice()).into())
                                .await;
                        });
                        (
                            [("content-type", "text/event-stream")],
                            axum::http::Response::new(body),
                        )
                    }),
                ),
        )?;
        let (red, _red) = frontend(&upstream)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let response = client
            .get(format!("{red}/api/codex/redirect"))
            .send()
            .await?;
        assert_eq!(response.status(), 307);
        assert_eq!(response.headers()["location"], "/outside");
        let mut response = client.get(format!("{red}/api/codex/events")).send().await?;
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let first = tokio::time::timeout(Duration::from_millis(100), response.chunk()).await??;
        assert_eq!(first.as_deref(), Some(b"data: one\n\n".as_slice()));
        assert_eq!(
            response.chunk().await?.as_deref(),
            Some(b"data: two\n\n".as_slice())
        );
        Ok(())
    }

    #[tokio::test]
    async fn rejects_upgrade_and_bounds_proxy_header_wait() -> Result<()> {
        let (upstream, _upstream) = serve(Router::new().fallback(post(|| async {
            tokio::time::sleep(Duration::from_millis(400)).await;
            "late"
        })))?;
        let (red, _red) = frontend(&upstream)?;
        let client = reqwest::Client::new();
        assert_eq!(
            client
                .get(format!("{red}/api/codex/events"))
                .header("upgrade", "websocket")
                .send()
                .await?
                .status(),
            501
        );
        let response = client.post(format!("{red}/api/codex/new")).send().await?;
        assert_eq!(response.status(), 503);
        assert_eq!(response.json::<Value>().await?["reason"], "timeout");
        Ok(())
    }

    #[test]
    fn prevents_path_escape_and_arbitrary_upstreams() -> Result<()> {
        for path in [
            "/api/codex/../health",
            "/api/codex/%2e%2e/health",
            "/api/codex/a%2F..%2F..%2Fhealth",
            "/api/codex/%252e%252e/health",
            "/api/codex/%5c..%5chealth",
            "/outside",
        ] {
            assert!(!safe_path(&path.parse()?));
        }
        for path in [
            "/api/codex",
            "/api/codex/",
            "/api/codex/a%2Fb?q=../outside",
            "/api/codex/http://other.example",
        ] {
            assert!(safe_path(&path.parse()?));
        }
        for origin in [
            "ftp://localhost",
            "http://user:pass@localhost",
            "http://localhost/api",
            "http://localhost?url=http://elsewhere",
        ] {
            assert!(Bridge::new(origin).is_err());
        }
        Ok(())
    }
}
