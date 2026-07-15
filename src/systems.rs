//! systems — the `/api/systems/*` scope: a maintenance/chore tracker with a
//! synthetic forcing function. A non-terminating "keep doing X" goal is turned
//! into an accruing *deficit* that pages you via ntfy, so a skipped habit has a
//! real (synthetic) deadline instead of silently rotting.
//!
//! Hard rule (see CONTRACT.md): deficit/fitness is NEVER stored — it's always
//! recomputed from the raw session log on read, so the model is restart-safe and
//! retunable. red takes NO app config beyond the redis connection; tracks,
//! tunables, schedules and ntfy settings all live in redis and are driven by the
//! UI. Single user, no auth (sits behind cluster ingress).
//!
//! Redis-backed like `countdowns.rs`: one shared `ConnectionManager` opened in
//! `setup_systems`, cloned into every handler and into the background pager.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, NaiveDate, Timelike, Utc};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use tracing::warn;

// --- Redis keys (namespace `systems:`) ---
const SETTINGS_KEY: &str = "systems:settings";
const TRACKS_KEY: &str = "systems:tracks";

fn sessions_key(id: &str) -> String {
    format!("systems:sessions:{id}")
}
fn skips_key(id: &str) -> String {
    format!("systems:skips:{id}")
}
fn scheduled_key(id: &str) -> String {
    format!("systems:scheduled:{id}")
}
fn last_alert_key(id: &str) -> String {
    format!("systems:last_alert:{id}")
}

/// The pager wakes on this fixed cadence (an impl constant, not config) and only
/// actually pages once per day per track (see `last_alert` dedupe).
const TICK: Duration = Duration::from_secs(15 * 60);

/// Adherence looks back this many local days (contract: "last 14 local days").
const ADHERENCE_DAYS: i64 = 14;

/// Cap the ntfy POST so a wedged ntfy can't hang a pager tick.
const NTFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the redis URL from the env the k8s Deployment injects, falling back to
/// a local redis for `cargo run` (identical to `countdowns.rs`).
fn redis_url() -> String {
    let host = std::env::var("REDIS_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("REDIS_PORT").unwrap_or_else(|_| "6379".to_string());
    format!("redis://{host}:{port}")
}

// --- JSON shapes ---

/// A tracked habit. `id` is a stable, url-safe slug the server derives from
/// `name` on create; `kind` fixes the logging modality and is immutable after.
#[derive(Serialize, Deserialize, Clone)]
struct Track {
    id: String,
    name: String,
    /// "box" | "check" | "loe".
    kind: String,
    tau_days: f64,
    /// Expected load per occurrence; drives the maintenance floor.
    mean_load: f64,
    period_days: f64,
    alert_mult: f64,
    /// Scheduled-obligation weekdays, 0=Sun … 6=Sat.
    weekdays: Vec<u32>,
    /// Load of one occurrence for `check`/`loe`; ignored for `box`.
    unit_load: f64,
    created_ms: i64,
}

/// Create body: everything but the server-assigned `id`/`created_ms`.
#[derive(Deserialize)]
struct CreateTrack {
    name: String,
    kind: String,
    tau_days: f64,
    mean_load: f64,
    period_days: f64,
    alert_mult: f64,
    weekdays: Vec<u32>,
    unit_load: f64,
}

/// Update body: the mutable fields only (`kind`/`id`/`created_ms` are immutable).
#[derive(Deserialize)]
struct UpdateTrack {
    name: String,
    tau_days: f64,
    mean_load: f64,
    period_days: f64,
    alert_mult: f64,
    weekdays: Vec<u32>,
    unit_load: f64,
}

/// Global settings. Empty `ntfy_url` disables paging entirely.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Settings {
    ntfy_url: String,
    dashboard_url: String,
    /// Local hour [0-23]; the pager only fires at/after this hour each day.
    alert_hour: u32,
    warmup_days: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ntfy_url: String::new(),
            dashboard_url: String::new(),
            alert_hour: 8,
            warmup_days: 90,
        }
    }
}

/// A logged session, as we read it back for recompute. Stored records also carry
/// their raw kind-specific fields, but the model only needs `ts_ms` + `load`
/// (serde ignores the rest).
#[derive(Deserialize)]
struct LoggedSession {
    ts_ms: i64,
    load: f64,
}

/// The record we persist per session: `ts_ms` + computed `load` + the raw fields
/// for the track's kind (so the log stays self-describing / auditable).
#[derive(Serialize)]
struct StoredSession {
    ts_ms: i64,
    load: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    intensity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    loe: Option<String>,
}

/// Kind-specific session POST body. Which fields are required depends on the
/// track's kind (see `compute_load`); `ts_ms` defaults to now.
#[derive(Deserialize)]
struct SessionBody {
    ts_ms: Option<i64>,
    duration_min: Option<f64>,
    intensity: Option<f64>,
    loe: Option<String>,
}

/// A skip of a scheduled obligation. A null/empty `reason` is an *unlogged* skip
/// — the relapse signal that pages on its own.
#[derive(Serialize, Deserialize)]
struct Skip {
    scheduled_date: String,
    reason: Option<String>,
    ts_ms: i64,
}

#[derive(Deserialize)]
struct SkipBody {
    scheduled_date: String,
    #[serde(default)]
    reason: Option<String>,
}

/// A materialized obligation (`systems:scheduled:<id>` hash value).
#[derive(Serialize)]
struct Scheduled {
    materialized_ms: i64,
}

#[derive(Serialize)]
struct LoadResp {
    load: f64,
}

#[derive(Serialize, Clone)]
struct DatePoint {
    date: String,
    value: f64,
}

#[derive(Serialize, Clone)]
struct LoadPoint {
    date: String,
    load: f64,
}

#[derive(Serialize, Clone)]
struct Adherence {
    scheduled: u32,
    completed: u32,
    unlogged_skips: u32,
}

#[derive(Serialize)]
struct Dashboard {
    id: String,
    name: String,
    kind: String,
    floor: f64,
    deficit_load: f64,
    deficit_sessions: f64,
    alert_threshold: f64,
    fitness: Vec<DatePoint>,
    daily_load: Vec<LoadPoint>,
    adherence: Adherence,
}

#[derive(Serialize)]
struct OverviewItem {
    id: String,
    name: String,
    kind: String,
    deficit_load: f64,
    deficit_sessions: f64,
    alert_threshold: f64,
    over_threshold: bool,
    unlogged_skips: u32,
    last_session_ms: Option<i64>,
}

// --- The model (pure; recomputed from the raw log on every read) ---

/// Everything the dashboard/overview/pager derive from a track's raw log. Never
/// persisted — always recomputed.
struct Computed {
    floor: f64,
    deficit_load: f64,
    deficit_sessions: f64,
    alert_threshold: f64,
    fitness: Vec<DatePoint>,
    daily_load: Vec<LoadPoint>,
    adherence: Adherence,
}

/// A Unix-ms instant's *local* calendar day (bucketing is local-day per the
/// contract). Via UTC so the instant→day mapping is always unambiguous.
fn local_date(ts_ms: i64) -> Option<NaiveDate> {
    DateTime::<Utc>::from_timestamp_millis(ts_ms).map(|dt| dt.with_timezone(&Local).date_naive())
}

/// `d` shifted `n` days earlier, saturating to `d` on the (unreachable) overflow.
fn days_before(d: NaiveDate, n: i64) -> NaiveDate {
    d.checked_sub_signed(ChronoDuration::days(n)).unwrap_or(d)
}

/// Load of one logged session, per the track's kind. `None` = the body is
/// missing a field the kind requires (caller returns 400).
fn compute_load(kind: &str, body: &SessionBody, unit_load: f64) -> Option<f64> {
    match kind {
        // High-variance/feely (e.g. workout): duration * intensity.
        "box" => match (body.duration_min, body.intensity) {
            (Some(d), Some(i)) => Some(d * i),
            _ => None,
        },
        // Binary "did it".
        "check" => Some(unit_load),
        // "Did it" at a level of effort.
        "loe" => match body.loe.as_deref() {
            Some("low") => Some(unit_load),
            Some("med") => Some(unit_load * 2.0),
            Some("high") => Some(unit_load * 3.0),
            _ => None,
        },
        _ => None,
    }
}

/// Recompute the full model for one track from its raw sessions/skips. `now` is
/// the local instant to anchor "today"/the warmup window on.
fn compute(
    track: &Track,
    sessions: &[LoggedSession],
    skips: &[Skip],
    settings: &Settings,
    now: DateTime<Local>,
) -> Computed {
    let warmup = settings.warmup_days.max(1);
    let today = now.date_naive();
    let start = days_before(today, warmup - 1);

    // Local-day bucket load `w_d` over the warmup window.
    let mut buckets: HashMap<NaiveDate, f64> = HashMap::new();
    for s in sessions {
        if let Some(d) = local_date(s.ts_ms) {
            if d >= start && d <= today {
                *buckets.entry(d).or_default() += s.load;
            }
        }
    }

    // Fitness EMA: f_0 = 0; f_d = f_{d-1}·exp(-1/tau) + w_d, over the window.
    let decay = (-1.0 / track.tau_days).exp();
    let mut prev = 0.0_f64;
    let mut fitness = Vec::new();
    let mut daily_load = Vec::new();
    let mut day = start;
    loop {
        let w = buckets.get(&day).copied().unwrap_or(0.0);
        let f = prev * decay + w;
        let date = day.format("%Y-%m-%d").to_string();
        fitness.push(DatePoint {
            date: date.clone(),
            value: f,
        });
        daily_load.push(LoadPoint { date, load: w });
        prev = f;
        if day >= today {
            break;
        }
        match day.succ_opt() {
            Some(n) => day = n,
            None => break,
        }
    }
    let f_today = prev;

    // Maintenance floor f* = mean_load / (1 − exp(−period/tau)); guard div-by-0.
    let denom = 1.0 - (-track.period_days / track.tau_days).exp();
    let floor = if denom > 0.0 {
        track.mean_load / denom
    } else {
        track.mean_load
    };
    let deficit_load = (floor - f_today).max(0.0);
    let deficit_sessions = if track.mean_load > 0.0 {
        deficit_load / track.mean_load
    } else {
        0.0
    };
    let alert_threshold = track.alert_mult * track.mean_load;

    let adherence = adherence(track, sessions, skips, today);

    Computed {
        floor,
        deficit_load,
        deficit_sessions,
        alert_threshold,
        fitness,
        daily_load,
        adherence,
    }
}

/// 14-day adherence, derived from the raw log (no stored state): scheduled days
/// come from `weekdays`, completed = scheduled days with a session, and
/// unlogged_skips = null/empty-reason skips in the window.
fn adherence(
    track: &Track,
    sessions: &[LoggedSession],
    skips: &[Skip],
    today: NaiveDate,
) -> Adherence {
    let start = days_before(today, ADHERENCE_DAYS - 1);
    let session_days: HashSet<NaiveDate> = sessions
        .iter()
        .filter_map(|s| local_date(s.ts_ms))
        .collect();

    let mut scheduled = 0_u32;
    let mut completed = 0_u32;
    let mut day = start;
    loop {
        if track
            .weekdays
            .contains(&day.weekday().num_days_from_sunday())
        {
            scheduled += 1;
            if session_days.contains(&day) {
                completed += 1;
            }
        }
        if day >= today {
            break;
        }
        match day.succ_opt() {
            Some(n) => day = n,
            None => break,
        }
    }

    let unlogged_skips = skips
        .iter()
        .filter(|s| is_unlogged(&s.reason) && in_window(s, start, today))
        .count() as u32;

    Adherence {
        scheduled,
        completed,
        unlogged_skips,
    }
}

/// null OR blank reason = the relapse signal.
fn is_unlogged(reason: &Option<String>) -> bool {
    reason
        .as_deref()
        .map(|r| r.trim().is_empty())
        .unwrap_or(true)
}

/// A skip's local day (its `scheduled_date`, or `ts_ms` if that won't parse)
/// falling within [start, today].
fn in_window(skip: &Skip, start: NaiveDate, today: NaiveDate) -> bool {
    let day = NaiveDate::parse_from_str(&skip.scheduled_date, "%Y-%m-%d")
        .ok()
        .or_else(|| local_date(skip.ts_ms));
    matches!(day, Some(d) if d >= start && d <= today)
}

// --- Redis helpers ---

async fn load_settings(conn: &mut ConnectionManager) -> redis::RedisResult<Settings> {
    let raw: Option<String> = conn.get(SETTINGS_KEY).await?;
    Ok(raw
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default())
}

async fn load_track(conn: &mut ConnectionManager, id: &str) -> redis::RedisResult<Option<Track>> {
    let raw: Option<String> = conn.hget(TRACKS_KEY, id).await?;
    Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
}

async fn load_tracks(conn: &mut ConnectionManager) -> redis::RedisResult<Vec<Track>> {
    let map: HashMap<String, String> = conn.hgetall(TRACKS_KEY).await?;
    let mut tracks: Vec<Track> = map
        .values()
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect();
    tracks.sort_by_key(|t| t.created_ms);
    Ok(tracks)
}

async fn load_sessions(
    conn: &mut ConnectionManager,
    id: &str,
) -> redis::RedisResult<Vec<LoggedSession>> {
    let raw: Vec<String> = conn.lrange(sessions_key(id), 0, -1).await?;
    Ok(raw
        .iter()
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect())
}

async fn load_skips(conn: &mut ConnectionManager, id: &str) -> redis::RedisResult<Vec<Skip>> {
    let raw: Vec<String> = conn.lrange(skips_key(id), 0, -1).await?;
    Ok(raw
        .iter()
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect())
}

/// One place to turn a redis error into a `502`, matching `countdowns.rs`.
fn redis_bad(ctx: &str, err: &redis::RedisError) -> Response {
    warn!("systems: redis {ctx}: {err}");
    StatusCode::BAD_GATEWAY.into_response()
}

// --- Handlers ---

/// GET /api/systems/settings -> Settings (defaults if unset).
async fn get_settings(State(mut conn): State<ConnectionManager>) -> Response {
    match load_settings(&mut conn).await {
        Ok(s) => Json(s).into_response(),
        Err(err) => redis_bad("read settings", &err),
    }
}

/// PUT /api/systems/settings -> stores the global settings.
async fn put_settings(
    State(mut conn): State<ConnectionManager>,
    Json(s): Json<Settings>,
) -> Response {
    let json = serde_json::to_string(&s).unwrap_or_default();
    match conn.set::<_, _, ()>(SETTINGS_KEY, json).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => redis_bad("write settings", &err),
    }
}

/// GET /api/systems/overview -> one summary row per track.
async fn get_overview(State(mut conn): State<ConnectionManager>) -> Response {
    let now = Local::now();
    let settings = match load_settings(&mut conn).await {
        Ok(s) => s,
        Err(err) => return redis_bad("read settings", &err),
    };
    let tracks = match load_tracks(&mut conn).await {
        Ok(t) => t,
        Err(err) => return redis_bad("read tracks", &err),
    };
    let mut items = Vec::with_capacity(tracks.len());
    for track in tracks {
        let sessions = match load_sessions(&mut conn, &track.id).await {
            Ok(s) => s,
            Err(err) => return redis_bad("read sessions", &err),
        };
        let skips = match load_skips(&mut conn, &track.id).await {
            Ok(s) => s,
            Err(err) => return redis_bad("read skips", &err),
        };
        let c = compute(&track, &sessions, &skips, &settings, now);
        items.push(OverviewItem {
            id: track.id,
            name: track.name,
            kind: track.kind,
            deficit_load: c.deficit_load,
            deficit_sessions: c.deficit_sessions,
            alert_threshold: c.alert_threshold,
            over_threshold: c.deficit_load > c.alert_threshold,
            unlogged_skips: c.adherence.unlogged_skips,
            last_session_ms: sessions.iter().map(|s| s.ts_ms).max(),
        });
    }
    Json(items).into_response()
}

/// GET /api/systems/tracks -> every track, oldest first.
async fn list_tracks(State(mut conn): State<ConnectionManager>) -> Response {
    match load_tracks(&mut conn).await {
        Ok(tracks) => Json(tracks).into_response(),
        Err(err) => redis_bad("read tracks", &err),
    }
}

/// POST /api/systems/tracks -> create. Server slugifies `name`->`id`, assigns
/// `created_ms`, and 409s if the id already exists (atomic via HSETNX).
async fn create_track(
    State(mut conn): State<ConnectionManager>,
    Json(body): Json<CreateTrack>,
) -> Response {
    let id = slugify(&body.name);
    if id.is_empty() {
        return (StatusCode::BAD_REQUEST, "name has no url-safe characters").into_response();
    }
    if !matches!(body.kind.as_str(), "box" | "check" | "loe") {
        return (StatusCode::BAD_REQUEST, "kind must be box|check|loe").into_response();
    }
    let track = Track {
        id: id.clone(),
        name: body.name,
        kind: body.kind,
        tau_days: body.tau_days,
        mean_load: body.mean_load,
        period_days: body.period_days,
        alert_mult: body.alert_mult,
        weekdays: body.weekdays,
        unit_load: body.unit_load,
        created_ms: Local::now().timestamp_millis(),
    };
    let json = serde_json::to_string(&track).unwrap_or_default();
    match conn.hset_nx::<_, _, _, bool>(TRACKS_KEY, &id, json).await {
        Ok(true) => Json(track).into_response(),
        Ok(false) => StatusCode::CONFLICT.into_response(),
        Err(err) => redis_bad("create track", &err),
    }
}

/// GET /api/systems/tracks/:id -> the track, or 404.
async fn get_track(State(mut conn): State<ConnectionManager>, Path(id): Path<String>) -> Response {
    match load_track(&mut conn, &id).await {
        Ok(Some(track)) => Json(track).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => redis_bad("read track", &err),
    }
}

/// PUT /api/systems/tracks/:id -> update mutable fields (kind/id/created_ms are
/// preserved from the existing track). 404 if missing.
async fn update_track(
    State(mut conn): State<ConnectionManager>,
    Path(id): Path<String>,
    Json(body): Json<UpdateTrack>,
) -> Response {
    let mut track = match load_track(&mut conn, &id).await {
        Ok(Some(t)) => t,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => return redis_bad("read track", &err),
    };
    track.name = body.name;
    track.tau_days = body.tau_days;
    track.mean_load = body.mean_load;
    track.period_days = body.period_days;
    track.alert_mult = body.alert_mult;
    track.weekdays = body.weekdays;
    track.unit_load = body.unit_load;
    let json = serde_json::to_string(&track).unwrap_or_default();
    match conn.hset::<_, _, _, ()>(TRACKS_KEY, &id, json).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => redis_bad("update track", &err),
    }
}

/// DELETE /api/systems/tracks/:id -> remove the track and all its per-track keys
/// (sessions/skips/scheduled/last_alert). Idempotent.
async fn delete_track(
    State(mut conn): State<ConnectionManager>,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = conn.hdel::<_, _, ()>(TRACKS_KEY, &id).await {
        return redis_bad("delete track", &err);
    }
    let keys = vec![
        sessions_key(&id),
        skips_key(&id),
        scheduled_key(&id),
        last_alert_key(&id),
    ];
    match conn.del::<_, ()>(keys).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => redis_bad("delete track data", &err),
    }
}

/// POST /api/systems/tracks/:id/session -> log a session, computing `load` from
/// the body per the track's kind. 404 if missing, 400 on a body the kind can't
/// score. Returns `{ load }`.
async fn log_session(
    State(mut conn): State<ConnectionManager>,
    Path(id): Path<String>,
    Json(body): Json<SessionBody>,
) -> Response {
    let track = match load_track(&mut conn, &id).await {
        Ok(Some(t)) => t,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => return redis_bad("read track", &err),
    };
    let Some(load) = compute_load(&track.kind, &body, track.unit_load) else {
        return (
            StatusCode::BAD_REQUEST,
            "session body invalid for track kind",
        )
            .into_response();
    };
    let ts_ms = body
        .ts_ms
        .unwrap_or_else(|| Local::now().timestamp_millis());
    let is_box = track.kind == "box";
    let is_loe = track.kind == "loe";
    let stored = StoredSession {
        ts_ms,
        load,
        duration_min: if is_box { body.duration_min } else { None },
        intensity: if is_box { body.intensity } else { None },
        loe: if is_loe { body.loe } else { None },
    };
    let json = serde_json::to_string(&stored).unwrap_or_default();
    match conn.lpush::<_, _, ()>(sessions_key(&id), json).await {
        Ok(()) => Json(LoadResp { load }).into_response(),
        Err(err) => redis_bad("log session", &err),
    }
}

/// POST /api/systems/tracks/:id/skip -> log a skip. Null/empty reason = the
/// relapse signal (an unlogged skip). 404 if the track is missing.
async fn log_skip(
    State(mut conn): State<ConnectionManager>,
    Path(id): Path<String>,
    Json(body): Json<SkipBody>,
) -> Response {
    match load_track(&mut conn, &id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => return redis_bad("read track", &err),
    }
    let skip = Skip {
        scheduled_date: body.scheduled_date,
        reason: body.reason,
        ts_ms: Local::now().timestamp_millis(),
    };
    let json = serde_json::to_string(&skip).unwrap_or_default();
    match conn.lpush::<_, _, ()>(skips_key(&id), json).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => redis_bad("log skip", &err),
    }
}

/// GET /api/systems/tracks/:id/dashboard -> the full per-track model. 404 if
/// missing.
async fn get_dashboard(
    State(mut conn): State<ConnectionManager>,
    Path(id): Path<String>,
) -> Response {
    let now = Local::now();
    let track = match load_track(&mut conn, &id).await {
        Ok(Some(t)) => t,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => return redis_bad("read track", &err),
    };
    let settings = match load_settings(&mut conn).await {
        Ok(s) => s,
        Err(err) => return redis_bad("read settings", &err),
    };
    let sessions = match load_sessions(&mut conn, &id).await {
        Ok(s) => s,
        Err(err) => return redis_bad("read sessions", &err),
    };
    let skips = match load_skips(&mut conn, &id).await {
        Ok(s) => s,
        Err(err) => return redis_bad("read skips", &err),
    };
    let c = compute(&track, &sessions, &skips, &settings, now);
    Json(Dashboard {
        id: track.id,
        name: track.name,
        kind: track.kind,
        floor: c.floor,
        deficit_load: c.deficit_load,
        deficit_sessions: c.deficit_sessions,
        alert_threshold: c.alert_threshold,
        fitness: c.fitness,
        daily_load: c.daily_load,
        adherence: c.adherence,
    })
    .into_response()
}

/// Lowercase, collapse each run of non-alphanumerics to a single `-`, and trim
/// leading/trailing `-` -> a stable, url-safe redis key.
fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !out.is_empty() && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

// --- Paging (behind a Notifier trait so a second channel is a drop-in) ---

/// One page's rendered content plus its delivery targets (topic + click URL come
/// from `settings`, so a settings change takes effect on the next tick).
struct Alert {
    topic_url: String,
    title: String,
    body: String,
    priority: String,
    tags: String,
    click: String,
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Delivery channel abstraction. Boxed-future method so it stays object-safe and
/// a second channel can be swapped in without touching the pager's call site.
trait Notifier: Send + Sync {
    fn notify<'a>(&'a self, alert: &'a Alert) -> BoxFuture<'a, ()>;
}

/// ntfy channel: a plain POST to the topic URL with title/priority/tags/click
/// headers. Failures are logged, never propagated (a page is best-effort).
///
/// `token` is the ntfy access token from `NTFY_TOKEN` (a k8s Secret, like the
/// existing `RCON_PASSWORD` env). ntfy runs `auth-default-access: deny-all`, so
/// without it every publish is rejected. This is a credential, not app config,
/// so it stays out of the redis-backed settings — the same split Alertmanager
/// uses (topic URL is config; the Bearer token is a mounted secret).
struct NtfyNotifier {
    http: reqwest::Client,
    token: Option<String>,
}

impl Notifier for NtfyNotifier {
    fn notify<'a>(&'a self, alert: &'a Alert) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let mut req = self
                .http
                .post(&alert.topic_url)
                .header("Title", alert.title.as_str())
                .header("Priority", alert.priority.as_str())
                .header("Tags", alert.tags.as_str())
                .body(alert.body.clone());
            if let Some(token) = &self.token {
                req = req.header("Authorization", format!("Bearer {token}"));
            }
            if !alert.click.is_empty() {
                req = req.header("Click", alert.click.as_str());
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => {}
                Ok(resp) => warn!("systems: ntfy returned {}", resp.status()),
                Err(err) => warn!("systems: ntfy post failed: {err}"),
            }
        })
    }
}

/// Render the page body: deficit is the primary signal; an unlogged skip is the
/// fallback (relapse) reason.
fn build_alert(settings: &Settings, track: &Track, c: &Computed) -> Alert {
    let body = if c.deficit_load > c.alert_threshold {
        format!(
            "{}: deficit ~{:.1} sessions below floor — schedule one and clear it.",
            track.name, c.deficit_sessions
        )
    } else {
        format!(
            "{}: {} unlogged skip(s) — log a reason or do it.",
            track.name, c.adherence.unlogged_skips
        )
    };
    Alert {
        topic_url: settings.ntfy_url.clone(),
        title: track.name.clone(),
        body,
        priority: "high".to_string(),
        tags: "warning".to_string(),
        click: settings.dashboard_url.clone(),
    }
}

/// Detached background pager. Ticks every `TICK`; each tick reads settings and,
/// if paging is enabled and it's past `alert_hour`, pages every over-threshold /
/// relapsing track once per day.
async fn paging_loop(mut conn: ConnectionManager, http: reqwest::Client) {
    // Read once at boot: an empty/absent token means unauthenticated posts (fine
    // for a permissive ntfy, rejected by our deny-all server — logged per tick).
    let token = std::env::var("NTFY_TOKEN").ok().filter(|t| !t.is_empty());
    let notifier: Box<dyn Notifier> = Box::new(NtfyNotifier { http, token });
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        if let Err(err) = tick_once(&mut conn, notifier.as_ref()).await {
            warn!("systems: pager tick failed: {err}");
        }
    }
}

/// A single pager tick. Redis errors bubble up (logged once by the loop) rather
/// than paging on a partial read.
async fn tick_once(conn: &mut ConnectionManager, notifier: &dyn Notifier) -> Result<()> {
    let settings = load_settings(conn).await?;
    if settings.ntfy_url.trim().is_empty() {
        return Ok(());
    }
    let now = Local::now();
    if now.hour() < settings.alert_hour {
        return Ok(());
    }
    let today = now.date_naive();
    let today_str = today.format("%Y-%m-%d").to_string();
    let today_wd = today.weekday().num_days_from_sunday();

    for track in load_tracks(conn).await? {
        // Materialize today's obligation once, if today is a scheduled weekday.
        if track.weekdays.contains(&today_wd) {
            let skey = scheduled_key(&track.id);
            let exists: bool = conn.hexists(&skey, &today_str).await?;
            if !exists {
                let val = serde_json::to_string(&Scheduled {
                    materialized_ms: now.timestamp_millis(),
                })
                .unwrap_or_default();
                conn.hset::<_, _, _, ()>(&skey, &today_str, val).await?;
            }
        }

        // Recompute; page on deficit over threshold OR any unlogged skip.
        let sessions = load_sessions(conn, &track.id).await?;
        let skips = load_skips(conn, &track.id).await?;
        let c = compute(&track, &sessions, &skips, &settings, now);
        let should_page = c.deficit_load > c.alert_threshold || c.adherence.unlogged_skips > 0;
        if !should_page {
            continue;
        }

        // Same-day dedupe: at most one page per track per local day.
        let lkey = last_alert_key(&track.id);
        let last: Option<String> = conn.get(&lkey).await?;
        if last.as_deref() == Some(today_str.as_str()) {
            continue;
        }
        notifier.notify(&build_alert(&settings, &track, &c)).await;
        conn.set::<_, _, ()>(&lkey, &today_str).await?;
    }
    Ok(())
}

/// Registers the `/api/systems/*` scope and spawns the detached pager. Opens one
/// shared, auto-reconnecting redis connection (cloned into every handler and the
/// pager), mirroring `countdowns.rs`.
pub async fn setup_systems(app: Router) -> Result<Router> {
    let client = redis::Client::open(redis_url())?;
    let conn = ConnectionManager::new(client).await?;
    // Shared, timeout-capped HTTP client for ntfy (build failure does no I/O).
    let http = reqwest::Client::builder().timeout(NTFY_TIMEOUT).build()?;

    tokio::spawn(paging_loop(conn.clone(), http));

    let routes = Router::new()
        .route("/api/systems/settings", get(get_settings).put(put_settings))
        .route("/api/systems/overview", get(get_overview))
        .route("/api/systems/tracks", get(list_tracks).post(create_track))
        .route(
            "/api/systems/tracks/:id",
            get(get_track).put(update_track).delete(delete_track),
        )
        .route("/api/systems/tracks/:id/session", post(log_session))
        .route("/api/systems/tracks/:id/skip", post(log_skip))
        .route("/api/systems/tracks/:id/dashboard", get(get_dashboard))
        .with_state(conn);

    Ok(app.merge(routes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> Track {
        Track {
            id: "exercise".to_string(),
            name: "Exercise".to_string(),
            kind: "box".to_string(),
            tau_days: 7.0,
            mean_load: 240.0,
            period_days: 2.0,
            alert_mult: 1.75,
            weekdays: vec![1, 3, 5, 0],
            unit_load: 60.0,
            created_ms: 0,
        }
    }

    #[test]
    fn slugify_makes_stable_url_safe_ids() {
        assert_eq!(slugify("Exercise"), "exercise");
        assert_eq!(slugify("  Take Vitamins! "), "take-vitamins");
        assert_eq!(slugify("A/B__C"), "a-b-c");
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn compute_load_per_kind() {
        let box_body = SessionBody {
            ts_ms: None,
            duration_min: Some(30.0),
            intensity: Some(8.0),
            loe: None,
        };
        assert_eq!(compute_load("box", &box_body, 60.0), Some(240.0));

        let empty = SessionBody {
            ts_ms: None,
            duration_min: None,
            intensity: None,
            loe: None,
        };
        assert_eq!(compute_load("check", &empty, 60.0), Some(60.0));
        // box needs both fields -> unscoreable.
        assert_eq!(compute_load("box", &empty, 60.0), None);

        let loe_body = SessionBody {
            ts_ms: None,
            duration_min: None,
            intensity: None,
            loe: Some("high".to_string()),
        };
        assert_eq!(compute_load("loe", &loe_body, 60.0), Some(180.0));
        assert_eq!(compute_load("loe", &empty, 60.0), None);
    }

    #[test]
    fn floor_and_deficit_recomputed_from_empty_log() {
        let t = track();
        let s = Settings::default();
        let now = Local::now();
        let c = compute(&t, &[], &[], &s, now);

        let expected_floor = 240.0 / (1.0 - (-2.0_f64 / 7.0).exp());
        assert!((c.floor - expected_floor).abs() < 1e-6);
        // No sessions -> fitness is 0 today -> full floor is the deficit.
        assert!((c.deficit_load - expected_floor).abs() < 1e-6);
        assert!((c.deficit_sessions - expected_floor / 240.0).abs() < 1e-9);
        assert!((c.alert_threshold - 1.75 * 240.0).abs() < 1e-9);
        // Series is exactly warmup_days long.
        assert_eq!(c.fitness.len(), s.warmup_days as usize);
        assert_eq!(c.daily_load.len(), s.warmup_days as usize);
    }

    #[test]
    fn today_session_lifts_fitness_and_shrinks_deficit() {
        let t = track();
        let s = Settings::default();
        let now = Local::now();
        let sessions = vec![LoggedSession {
            ts_ms: now.timestamp_millis(),
            load: 300.0,
        }];
        let c = compute(&t, &sessions, &[], &s, now);
        let empty = compute(&t, &[], &[], &s, now);
        assert!(c.deficit_load < empty.deficit_load);
        // Today's bucket carries the load.
        let last = c.daily_load.last().map(|p| p.load).unwrap_or(0.0);
        assert!((last - 300.0).abs() < 1e-9);
    }

    #[test]
    fn only_blank_reason_skips_count_as_unlogged() {
        let t = track();
        let s = Settings::default();
        let now = Local::now();
        let today = now.date_naive().format("%Y-%m-%d").to_string();
        let skips = vec![
            Skip {
                scheduled_date: today.clone(),
                reason: None,
                ts_ms: now.timestamp_millis(),
            },
            Skip {
                scheduled_date: today.clone(),
                reason: Some("   ".to_string()),
                ts_ms: now.timestamp_millis(),
            },
            Skip {
                scheduled_date: today,
                reason: Some("sick".to_string()),
                ts_ms: now.timestamp_millis(),
            },
        ];
        let c = compute(&t, &[], &skips, &s, now);
        assert_eq!(c.adherence.unlogged_skips, 2);
    }
}
