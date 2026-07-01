use anyhow::{Context, Result};
use axum::{http::StatusCode, response::IntoResponse, routing::get, Router};
use std::net::SocketAddr;
use tokio::task::JoinSet;
use tracing::info;

use crate::Config;

async fn root() -> impl IntoResponse {
    (StatusCode::OK, "red")
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

pub async fn start_server(join_set: &mut JoinSet<Result<()>>, config: Config) -> Result<()> {
    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health));

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    info!("Server listening on http://{}", addr);

    join_set.spawn(async move {
        axum::Server::bind(&addr)
            .serve(app.into_make_service())
            .with_graceful_shutdown(shutdown_signal())
            .await
            .context("Http server stopped serving")
    });

    Ok(())
}

/// Resolves on Ctrl-C (SIGINT) or SIGTERM (what k8s sends on pod stop), letting
/// the server stop accepting new connections and drain in-flight requests
/// instead of being hard-killed.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            // Couldn't register the handler -> never resolve this branch.
            Err(err) => {
                tracing::warn!("Couldn't listen for SIGTERM: {err}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("Shutdown signal received, draining...");
}
