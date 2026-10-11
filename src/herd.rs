use axum::{routing::get, Json, Router};
use serde::Serialize;

#[derive(Serialize)]
struct HerdStatus {
    status: &'static str,
}

async fn status() -> Json<HerdStatus> {
    Json(HerdStatus { status: "ready" })
}

/// The first Herd API surface. Exercise playback and session state stay in the
/// browser until shared progress has a defined storage and identity contract.
pub fn setup_herd(app: Router) -> Router {
    app.route("/api/herd/status", get(status))
}
