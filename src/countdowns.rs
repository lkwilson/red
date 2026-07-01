use anyhow::Result;
use axum::{extract::Path, http::StatusCode, response::IntoResponse, routing::get, Json, Router};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde::Deserialize;
use std::collections::HashMap;
use tracing::warn;

/// Redis hash holding every countdown, keyed by name -> date. Kept as `hub:countdowns`
/// (not renamed to `red:*`) so data written while this lived on hub carries over.
const COUNTDOWNS_KEY: &str = "hub:countdowns";

#[derive(Deserialize)]
struct Countdown {
    name: String,
    /// Target date, stored as-is (e.g. ISO 8601 like "2026-12-25").
    date: String,
}

/// Build the redis URL from the env the k8s Deployment injects
/// (REDIS_HOST=db-redis-service, REDIS_PORT=6379), falling back to a local redis
/// for `cargo run`.
fn redis_url() -> String {
    let host = std::env::var("REDIS_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("REDIS_PORT").unwrap_or_else(|_| "6379".to_string());
    format!("redis://{host}:{port}")
}

/// GET /api/countdowns -> every countdown as a `{ name: date }` map.
async fn handle_list(mut conn: ConnectionManager) -> impl IntoResponse {
    let res: redis::RedisResult<HashMap<String, String>> = conn.hgetall(COUNTDOWNS_KEY).await;
    match res {
        Ok(map) => Json(map).into_response(),
        Err(err) => {
            warn!("Failed to read countdowns from redis: {err}");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// GET /api/countdowns/:name -> the one countdown's date, or 404.
async fn handle_get(Path(name): Path<String>, mut conn: ConnectionManager) -> impl IntoResponse {
    let res: redis::RedisResult<Option<String>> = conn.hget(COUNTDOWNS_KEY, &name).await;
    match res {
        Ok(Some(date)) => Json(date).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            warn!("Failed to read countdown {name} from redis: {err}");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// POST /api/countdowns  `{ "name": ..., "date": ... }` -> stores/overwrites it.
async fn handle_put(Json(cd): Json<Countdown>, mut conn: ConnectionManager) -> impl IntoResponse {
    let res: redis::RedisResult<i64> = conn.hset(COUNTDOWNS_KEY, &cd.name, &cd.date).await;
    match res {
        Ok(_) => StatusCode::OK.into_response(),
        Err(err) => {
            warn!("Failed to store countdown {} in redis: {err}", cd.name);
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// DELETE /api/countdowns/:name -> removes it (idempotent).
async fn handle_delete(Path(name): Path<String>, mut conn: ConnectionManager) -> impl IntoResponse {
    let res: redis::RedisResult<i64> = conn.hdel(COUNTDOWNS_KEY, &name).await;
    match res {
        Ok(_) => StatusCode::OK.into_response(),
        Err(err) => {
            warn!("Failed to delete countdown {name} from redis: {err}");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// Registers the redis-backed countdown store. Opens one shared, auto-reconnecting
/// connection (ConnectionManager) and clones it into each handler.
pub async fn setup_countdowns(app: Router) -> Result<Router> {
    let client = redis::Client::open(redis_url())?;
    let conn = ConnectionManager::new(client).await?;
    Ok(app
        .route(
            "/api/countdowns",
            get({
                let conn = conn.clone();
                move || handle_list(conn.clone())
            })
            .post({
                let conn = conn.clone();
                move |body| handle_put(body, conn.clone())
            }),
        )
        .route(
            "/api/countdowns/:name",
            get({
                let conn = conn.clone();
                move |path| handle_get(path, conn.clone())
            })
            .delete({
                let conn = conn.clone();
                move |path| handle_delete(path, conn.clone())
            }),
        ))
}
