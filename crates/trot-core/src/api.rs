//! TROT's local HTTP + WebSocket API (axum). This is the engine's public
//! contract — the stable surface the `trot` CLI and the Nowhere UI consume.
//! Presentation-agnostic: it serves JSON + a `/ws` stream, never a UI.

use crate::app::AppState;
use crate::ble;
use crate::db::{RETENTION_DAYS, ROLLUP_INTERVAL_S};
use crate::telemetry::{speed_kmh, speed_mph};
use axum::extract::{DefaultBodyLimit, Request};
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::{self, Next};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{header, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

/// CORS for the Nowhere UI, which loads from Tauri's own asset origin (a
/// different origin than this loopback daemon). We allow ONLY the Tauri webview
/// origins — never a website origin — so a page in a normal browser still can't
/// read responses; writes additionally require the token (see `guard`).
fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _req| {
            is_allowed_origin(origin)
        }))
        .allow_methods(Any)
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            HeaderName::from_static("x-sc110-token"),
        ])
}

/// Which browser origins may read the loopback API. Production Nowhere loads the
/// UI from Tauri's own origin; `tauri dev` serves it from a vite dev server on a
/// dynamic localhost port. Allow both. Safe because a remote site's Origin is its
/// own domain (never localhost), and state-changing calls still require the
/// per-launch token plus a loopback Host header (see `guard`).
fn is_allowed_origin(origin: &HeaderValue) -> bool {
    let o = match origin.to_str() {
        Ok(s) => s,
        Err(_) => return false,
    };
    // Tauri's production webview origins (macOS uses tauri://; Windows/Linux the
    // http(s)://tauri.localhost custom-protocol host).
    if matches!(
        o,
        "tauri://localhost" | "http://tauri.localhost" | "https://tauri.localhost"
    ) {
        return true;
    }
    // A localhost dev server (vite / tauri dev), any scheme or port.
    matches!(
        origin_host(o),
        Some("localhost") | Some("127.0.0.1") | Some("::1")
    )
}

/// Host portion of a `scheme://host[:port]` origin (brackets stripped for IPv6).
fn origin_host(origin: &str) -> Option<&str> {
    let authority = origin.split_once("://")?.1.split('/').next().unwrap_or("");
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or("") // [::1]:port
    } else {
        authority
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(authority)
    };
    (!host.is_empty()).then_some(host)
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/state", get(api_state))
        .route("/api/today", get(api_today))
        .route("/api/analytics", get(api_analytics))
        .route("/api/steps/by-device", get(api_steps_by_device))
        .route("/api/timeofday", get(api_timeofday))
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions/:id", get(api_session_detail))
        .route("/api/health", get(api_health))
        .route("/api/shutdown", post(api_shutdown))
        .route("/api/scan", get(api_scan))
        .route("/api/pair", post(api_pair))
        .route("/api/unpair", post(api_unpair))
        .route("/api/connect", post(api_connect))
        .route("/api/disconnect", post(api_disconnect))
        .route("/api/devices", get(api_devices))
        .route("/api/devices/active", post(api_device_activate))
        .route("/api/devices/forget", post(api_device_forget))
        .route("/api/rollup/status", get(api_rollup_status))
        .route("/api/rollup/run", post(api_rollup_run))
        .route("/api/export", get(api_export))
        .route("/api/diag", get(api_diag))
        .route("/api/diagnose", axum::routing::post(diagnose_start))
        .route(
            "/api/diagnose/:id",
            get(diagnose_snapshot).delete(diagnose_stop),
        )
        .route("/api/mark/speed", post(api_mark_speed))
        .route("/api/data/snapshot", get(api_data_snapshot))
        .route("/api/data/reset", post(api_data_reset))
        .route("/api/data/restore", post(api_data_restore))
        .route(
            "/api/settings",
            get(api_settings_get).post(api_settings_set),
        )
        .route("/api/import", post(api_import))
        .route("/ws", get(ws_handler))
        // Security guard (runs first): rejects non-loopback Host headers
        // (defeats DNS-rebinding), rejects disallowed browser Origins (covers the
        // /ws upgrade, which CORS does not), and requires the per-launch token on
        // state-changing /api calls (stops other local processes / cross-site
        // writes that could pair BLE devices or wipe the database).
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        // Body cap bounds memory from a hostile import while still allowing real
        // (multi-MB) backups.
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        // Restrictive CORS (only the Tauri/localhost origins in `is_allowed_origin`)
        // is outermost so a cross-site page can't read responses, and preflight
        // (OPTIONS) is answered before routing.
        .layer(cors())
        .with_state(state)
}

/// Per-request security guard. See router() for rationale.
async fn guard(State(s): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    // 1. Defeat DNS-rebinding: only serve requests whose Host is loopback. A
    //    rebound request from a malicious page carries the attacker's hostname.
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|h| {
            let host = if let Some(rest) = h.strip_prefix('[') {
                rest.split(']').next().unwrap_or("") // IPv6 literal, e.g. [::1]:port
            } else {
                h.rsplit_once(':').map(|(a, _)| a).unwrap_or(h)
            };
            matches!(host, "127.0.0.1" | "localhost" | "::1")
        })
        .unwrap_or(false);
    if !host_ok {
        return (StatusCode::FORBIDDEN, "bad host").into_response();
    }

    // 1b. Reject any request that carries a disallowed browser Origin. CORS stops
    //     a cross-site page from *reading* normal responses, but it does NOT cover
    //     the /ws upgrade — so we enforce the same origin allow-list here, for
    //     every request. Non-browser clients (the CLI / ureq) send no Origin and
    //     pass through untouched.
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        if !is_allowed_origin(origin) {
            return (StatusCode::FORBIDDEN, "bad origin").into_response();
        }
    }

    // 2. Require the session token on mutating API requests. Reads stay open so
    //    same-origin GETs work without ceremony; CORS + the Origin guard above
    //    already prevent cross-site reading of responses. Note this means any
    //    process running as YOU can read your activity data if it discovers the
    //    port — which is no worse than it reading the SQLite file directly. Other
    //    users are kept out by the 0700 data dir / 0600 handshake. See README.
    let is_write = matches!(
        *req.method(),
        Method::POST | Method::PUT | Method::DELETE | Method::PATCH
    );
    if (is_write && req.uri().path().starts_with("/api/"))
        || req.uri().path().starts_with("/api/diagnose")
    {
        let tok = req
            .headers()
            .get("x-sc110-token")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        if tok.is_empty() || tok != s.token {
            return (StatusCode::FORBIDDEN, "missing or invalid session token").into_response();
        }
    }

    let mut resp = next.run(req).await;
    // We only ever serve JSON. Stop a browser from content-sniffing a response
    // into something executable.
    resp.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

// ---- REST ------------------------------------------------------------------

async fn api_state(State(s): State<Arc<AppState>>) -> Json<Value> {
    Json(s.snapshot())
}

async fn api_today(State(s): State<Arc<AppState>>) -> Json<Value> {
    Json(s.today_response())
}

async fn api_health(State(s): State<Arc<AppState>>) -> Json<Value> {
    // `version` is the engine's own version, which is not the same thing as the
    // app's — desktop ships the engine as a separate sidecar binary that can be
    // older than the app bundling it. Exposing it on the cheapest endpoint (not
    // only in the /api/diag dump) lets a client show both without asking a user
    // to run a diagnostic.
    Json(json!({
        "ok": true,
        "connected": s.is_connected(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Gracefully disconnect the treadmill and stop the BLE worker, then respond.
/// Nowhere calls this on app-quit *before* killing the engine process: it blocks
/// until the SC110's BLE link is actually torn down, so the treadmill gets a real
/// GATT disconnect instead of a yanked socket (which it would otherwise hold onto
/// until power-cycled). Token-guarded (it's a POST), so only the paired UI can
/// trigger it. Terminal — the worker does not reconnect afterwards.
async fn api_shutdown(State(s): State<Arc<AppState>>) -> Json<Value> {
    s.shutdown(std::time::Duration::from_secs(3)).await;
    Json(json!({"ok": true}))
}

fn resolution_seconds(res: &str) -> Option<i64> {
    Some(match res {
        "minute" => 60,
        "5min" => 300,
        "15min" => 900,
        "30min" => 1800,
        "hour" => 3600,
        "6hour" => 21600,
        "day" => 86400,
        _ => return None,
    })
}

#[derive(Deserialize)]
struct AnalyticsParams {
    #[serde(default = "default_metric")]
    metric: String,
    #[serde(default = "default_resolution")]
    resolution: String,
    #[serde(default = "default_range")]
    range_days: f64,
}
fn default_metric() -> String {
    "steps".into()
}
fn default_resolution() -> String {
    "hour".into()
}
fn default_range() -> f64 {
    1.0
}

async fn api_analytics(
    State(s): State<Arc<AppState>>,
    Query(p): Query<AnalyticsParams>,
) -> Response {
    const METRICS: &[&str] = &[
        "steps",
        "calories",
        "distance_raw",
        "speed_raw",
        "duration_running_s",
    ];
    if !METRICS.contains(&p.metric.as_str()) {
        return (StatusCode::BAD_REQUEST, "bad metric").into_response();
    }
    let res_s = match resolution_seconds(&p.resolution) {
        Some(r) => r,
        None => return (StatusCode::BAD_REQUEST, "bad resolution").into_response(),
    };
    if p.range_days <= 0.0 || p.range_days > 365.0 * 5.0 {
        return (StatusCode::BAD_REQUEST, "range_days out of bounds").into_response();
    }
    // Bound the work: `range ÷ resolution` is the number of buckets SQLite has to
    // aggregate and we have to serialize. Reject absurd combinations (e.g.
    // 5 years at minute resolution ≈ 2.6M buckets) so a cheap request can't turn
    // into a CPU/memory amplifier.
    const MAX_BUCKETS: f64 = 200_000.0;
    if p.range_days * 86400.0 / res_s as f64 > MAX_BUCKETS {
        return (
            StatusCode::BAD_REQUEST,
            "range too large for this resolution — narrow range_days or use a coarser resolution",
        )
            .into_response();
    }
    let end_ts = crate::db::now_ts();
    let start_ts = end_ts - p.range_days * 86400.0;
    let raw = match s.db.timeseries(&p.metric, res_s, start_ts, end_ts) {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let unit = s.display_unit();
    let enriched: Vec<Value> = raw
        .into_iter()
        .map(|row| {
            let bucket_ts = row["bucket_ts"].clone();
            let value = row["value"].as_f64().unwrap_or(0.0);
            let mut entry = json!({"bucket_ts": bucket_ts, "value": value});
            if p.metric == "speed_raw" && value != 0.0 {
                let raw_int = value.round() as u32;
                entry["speed_kmh"] = json!(speed_kmh(raw_int, &unit));
                entry["speed_mph"] = json!(speed_mph(raw_int, &unit));
            } else if p.metric == "distance_raw" {
                entry["distance_m"] = json!(value * 10.0);
            }
            entry
        })
        .collect();
    Json(json!({
        "metric": p.metric,
        "resolution": p.resolution,
        "resolution_s": res_s,
        "range_days": p.range_days,
        "start_ts": start_ts,
        "end_ts": end_ts,
        "display_unit": &unit,
        "buckets": enriched,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct TimeOfDayParams {
    date: Option<String>,
    until_sod: Option<i64>,
}

/// Cumulative walking on a LOCAL `date` up to `until_sod` seconds since that
/// day's local midnight — for "steps vs the same point on a previous day".
/// Read-only (no token), served from the durable 1-minute rollups (correct even
/// after raw is pruned). `{steps, distance_raw}`; a data-less date is 0, not 404.
async fn api_timeofday(
    State(s): State<Arc<AppState>>,
    Query(p): Query<TimeOfDayParams>,
) -> Response {
    let date = match p.date {
        Some(d) => d,
        None => return (StatusCode::BAD_REQUEST, "date is required").into_response(),
    };
    // Validate the local-date shape (same "YYYY-MM-DD" convention as sessions).
    if chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").is_err() {
        return (StatusCode::BAD_REQUEST, "date must be YYYY-MM-DD").into_response();
    }
    let until_sod = match p.until_sod {
        Some(u) if (0..=90_000).contains(&u) => u,
        Some(_) => return (StatusCode::BAD_REQUEST, "until_sod out of range").into_response(),
        None => return (StatusCode::BAD_REQUEST, "until_sod is required").into_response(),
    };
    match s.db.timeofday_totals(&date, until_sod) {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct StepsByDeviceParams {
    days: Option<i64>,
}

/// Daily step totals split by the device that recorded them, over the last
/// `days` local days (default 30, capped 1..=400).
async fn api_steps_by_device(
    State(s): State<Arc<AppState>>,
    Query(p): Query<StepsByDeviceParams>,
) -> Response {
    let days = p.days.unwrap_or(30).clamp(1, 400);
    let since = (chrono::Local::now().date_naive() - chrono::Duration::days(days - 1))
        .format("%Y-%m-%d")
        .to_string();
    match s.db.steps_by_device(&since) {
        Ok(rows) => {
            // These totals come from the per-minute rollups, so the current
            // (un-rolled) minutes aren't in them yet. Tell the client how far the
            // data is actually complete instead of letting it present a slightly
            // low "today" as final.
            let complete_through_ts =
                s.db.rollup_status()
                    .ok()
                    .and_then(|st| st.get("last_rolled_ts").and_then(|v| v.as_f64()));
            Json(json!({
                "since": since,
                "days": days,
                "rows": rows,
                "complete_through_ts": complete_through_ts,
            }))
            .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct SessionsParams {
    #[serde(default = "default_limit")]
    limit: i64,
}
fn default_limit() -> i64 {
    50
}

async fn api_sessions(
    State(s): State<Arc<AppState>>,
    Query(p): Query<SessionsParams>,
) -> Json<Value> {
    let rows = s.db.list_sessions(p.limit).unwrap_or_default();
    Json(json!({"sessions": rows}))
}

async fn api_session_detail(State(s): State<Arc<AppState>>, Path(id): Path<i64>) -> Json<Value> {
    match s.db.get_session(id).ok().flatten() {
        Some(sess) => Json(serde_json::to_value(sess).unwrap_or(Value::Null)),
        None => Json(json!({"error": "not_found"})),
    }
}

#[derive(Deserialize)]
struct ScanParams {
    #[serde(default = "default_scan_secs")]
    seconds: f64,
    #[serde(default)]
    all_devices: bool,
}
fn default_scan_secs() -> f64 {
    6.0
}

async fn api_scan(Query(p): Query<ScanParams>) -> Response {
    match ble::scan(p.seconds, p.all_devices).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct PairBody {
    device_id: String,
    #[serde(default)]
    name: Option<String>,
}

fn devices_payload() -> Value {
    let cfg = crate::config::load_devices();
    let active = cfg.active.clone();
    let devices: Vec<Value> = cfg
        .devices
        .iter()
        .map(|d| {
            json!({
                "id": d.id,
                "name": d.name,
                "last_seen": d.last_seen,
                "active": Some(&d.id) == active.as_ref(),
            })
        })
        .collect();
    json!({"devices": devices, "active": active})
}

async fn api_devices() -> Json<Value> {
    Json(devices_payload())
}

async fn api_pair(State(s): State<Arc<AppState>>, Json(b): Json<PairBody>) -> Response {
    let id = b.device_id.trim().to_string();
    if id.is_empty() {
        return (StatusCode::BAD_REQUEST, "device_id must not be empty").into_response();
    }
    crate::config::add_and_activate(&id, b.name.as_deref());
    s.set_device_id(Some(id.clone()));
    let mut out = devices_payload();
    if let Value::Object(ref mut m) = out {
        m.insert("ok".into(), json!(true));
        m.insert("device_id".into(), json!(id));
    }
    Json(out).into_response()
}

#[derive(Deserialize)]
struct DeviceIdBody {
    device_id: String,
}

async fn api_device_activate(
    State(s): State<Arc<AppState>>,
    Json(b): Json<DeviceIdBody>,
) -> Response {
    let id = b.device_id.trim().to_string();
    if !crate::config::set_active(&id) {
        return (StatusCode::NOT_FOUND, "unknown device").into_response();
    }
    s.set_device_id(Some(id));
    Json(devices_payload()).into_response()
}

async fn api_device_forget(
    State(s): State<Arc<AppState>>,
    Json(b): Json<DeviceIdBody>,
) -> Json<Value> {
    let new_active = crate::config::forget(b.device_id.trim());
    s.set_device_id(new_active);
    Json(devices_payload())
}

async fn api_unpair(State(s): State<Arc<AppState>>) -> Json<Value> {
    // "Unpair" = forget the currently active treadmill.
    if let Some(active) = s.device_id() {
        let new_active = crate::config::forget(&active);
        s.set_device_id(new_active);
    }
    Json(devices_payload())
}

/// Manually drop the BLE link but stay paired and keep the engine running (so
/// cloud sync still works), then idle until `/api/connect`. Distinct from
/// `/api/shutdown` (terminal) and `/api/devices/forget` (unpair) — this is a
/// reversible disconnect that leaves the treadmill saved.
async fn api_disconnect(State(s): State<Arc<AppState>>) -> Json<Value> {
    s.set_paused(true);
    Json(json!({"ok": true, "connected": false, "paused": true}))
}

/// Resume after a manual disconnect: reconnect to the active paired treadmill.
async fn api_connect(State(s): State<Arc<AppState>>) -> Json<Value> {
    s.set_paused(false);
    Json(json!({"ok": true, "paused": false}))
}

async fn api_rollup_status(State(s): State<Arc<AppState>>) -> Json<Value> {
    let mut status = s.db.rollup_status().unwrap_or_else(|_| json!({}));
    if let Value::Object(ref mut m) = status {
        m.insert("retention_days".into(), json!(RETENTION_DAYS));
        m.insert("rollup_interval_s".into(), json!(ROLLUP_INTERVAL_S));
    }
    Json(status)
}

#[derive(Deserialize)]
struct RollupRunParams {
    #[serde(default = "default_true")]
    prune: bool,
    /// Non-destructively (re)compute rollup buckets from the full raw range
    /// (repairs buckets the old MAX-MIN writer corrupted) — never deletes buckets
    /// whose raw is gone. Skips pruning so the raw it rebuilds from stays.
    #[serde(default)]
    rebuild: bool,
}
fn default_true() -> bool {
    true
}

async fn api_rollup_run(
    State(s): State<Arc<AppState>>,
    Query(p): Query<RollupRunParams>,
) -> Json<Value> {
    if p.rebuild {
        // Safe full-history backfill over the entire raw range (0..now): upserts
        // buckets only where raw exists, never deletes — data-loss-free once raw
        // pruning is real.
        let res =
            s.db.backfill_rollups(0.0, crate::db::now_ts())
                .unwrap_or_else(|_| json!({}));
        let mut out = json!({"ok": true, "pruned_samples": 0, "rebuilt": true});
        if let (Value::Object(ref mut o), Value::Object(r)) = (&mut out, res) {
            for (k, v) in r {
                o.insert(k, v);
            }
        }
        return Json(out);
    }
    let res = s.db.rollup_samples().unwrap_or_else(|_| json!({}));
    let mut pruned = 0usize;
    if p.prune {
        pruned =
            s.db.prune_raw_samples(RETENTION_DAYS * 86400.0)
                .unwrap_or(0);
    }
    let mut out = json!({"ok": true, "pruned_samples": pruned});
    if let (Value::Object(ref mut o), Value::Object(r)) = (&mut out, res) {
        for (k, v) in r {
            o.insert(k, v);
        }
    }
    Json(out)
}

#[derive(Deserialize)]
struct ExportParams {
    /// `include=raw` re-adds the full raw `samples` array (manual backup). The
    /// default export is sessions + rollups_1m + speed_marks only — no raw.
    #[serde(default)]
    include: String,
    /// Bound the raw samples to `ts >= since`. Live sync uses this to ship the
    /// last few minutes — the part `day_totals` reads from raw and that a
    /// rollups-only export leaves behind — without pushing the whole day.
    #[serde(default)]
    since: Option<f64>,
}

async fn api_export(State(s): State<Arc<AppState>>, Query(p): Query<ExportParams>) -> Response {
    let include_raw = p.include.eq_ignore_ascii_case("raw");
    let dump =
        s.db.export_since(include_raw, p.since)
            .unwrap_or_else(|_| json!({}));
    let body = serde_json::to_string(&dump).unwrap_or_default();
    let filename = format!(
        "lifespan-sc110-{}.json",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        body,
    )
        .into_response()
}

/// Snapshot all data to a file, then wipe the live DB — lets the user test the
/// first-run/empty state and reload their data afterwards. The snapshot persists
/// across restarts and reinstalls (same data dir).
async fn api_data_reset(State(s): State<Arc<AppState>>) -> Json<Value> {
    // Full snapshot includes raw so a reset→restore round-trip loses nothing.
    let dump = match s.db.export_all(true) {
        Ok(d) => d,
        Err(e) => return Json(json!({"ok": false, "error": format!("export failed: {e}")})),
    };
    // Refuse to reset an already-empty DB: otherwise a second reset would export
    // nothing and overwrite the *first* reset's good snapshot with an empty one,
    // making the earlier data unrecoverable. Restore first if that's the intent.
    let count = |k: &str| {
        dump.get(k)
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    };
    let has_data = ["sessions", "samples", "rollups_1m", "speed_marks"]
        .iter()
        .any(|k| count(k) > 0);
    if !has_data {
        return Json(json!({
            "ok": false,
            "error": "nothing to reset — the database is already empty (use restore to recover a prior snapshot)"
        }));
    }
    let path = crate::config::snapshot_path();
    if let Err(e) =
        crate::config::atomic_write(&path, &serde_json::to_vec(&dump).unwrap_or_default())
    {
        return Json(json!({"ok": false, "error": format!("could not save snapshot: {e}")}));
    }
    if let Err(e) = s.db.wipe_all() {
        return Json(json!({"ok": false, "error": format!("wipe failed: {e}")}));
    }
    s.set_active_session(None);
    s.set_last_state(None);
    s.invalidate_today(); // the DB is now empty; don't serve cached totals
                          // Genuine fresh-install state: forget the paired device and re-arm the
                          // first-run wizard, so reopening the app starts setup from scratch.
    crate::config::clear_devices();
    let mut st = crate::config::load_settings();
    st.setup_complete = false;
    crate::config::save_settings(&st);
    s.set_device_id(None); // stops the worker and wakes it to wait for re-pair
    Json(
        json!({"ok": true, "snapshot_sessions": count("sessions"), "snapshot_samples": count("samples")}),
    )
}

#[derive(Deserialize)]
struct SettingsPatch {
    locale: Option<String>,
    display_unit: Option<String>,
    setup_complete: Option<bool>,
    device_name: Option<String>,
}

/// First-run / preferences state (locale, unit, whether setup is done).
async fn api_settings_get(State(s): State<Arc<AppState>>) -> Json<Value> {
    let st = crate::config::load_settings();
    Json(json!({
        "locale": st.locale,
        "display_unit": st.display_unit,
        "setup_complete": st.setup_complete,
        "needs_setup": !st.setup_complete,
        "active_device": s.device_id(),
        "device_name": st.device_name,
    }))
}

/// Patch any subset of settings. Changing the unit applies immediately (live).
async fn api_settings_set(
    State(s): State<Arc<AppState>>,
    Json(p): Json<SettingsPatch>,
) -> Json<Value> {
    let mut st = crate::config::load_settings();
    if let Some(l) = p.locale {
        st.locale = if l == "de" { "de".into() } else { "en".into() };
    }
    if let Some(u) = p.display_unit {
        let u = if u.to_lowercase() == "mph" {
            "mph".to_string()
        } else {
            "km/h".to_string()
        };
        st.display_unit = u.clone();
        s.set_display_unit(&u);
    }
    if let Some(c) = p.setup_complete {
        st.setup_complete = c;
    }
    if let Some(n) = p.device_name {
        // This label is stored on every session row and echoed back in the
        // by-device breakdown, so keep it to printable text: strip control
        // characters (newlines included) before trimming and capping.
        st.device_name = n
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>()
            .trim()
            .chars()
            .take(40)
            .collect();
    }
    crate::config::save_settings(&st);
    Json(json!({
        "ok": true,
        "locale": st.locale,
        "display_unit": st.display_unit,
        "setup_complete": st.setup_complete,
        "device_name": st.device_name,
    }))
}

/// Restore the data saved by the last reset (replace mode).
async fn api_data_restore(State(s): State<Arc<AppState>>) -> Json<Value> {
    let path = crate::config::snapshot_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            return Json(json!({"ok": false, "error": format!("no snapshot to restore: {e}")}))
        }
    };
    let dump: Value = match serde_json::from_slice(&bytes) {
        Ok(d) => d,
        Err(e) => return Json(json!({"ok": false, "error": format!("snapshot unreadable: {e}")})),
    };
    match s.db.import_dump(&dump, "replace") {
        Ok(res) => {
            s.invalidate_today(); // history replaced; recompute totals now
            let mut out = json!({"ok": true});
            if let (Value::Object(o), Value::Object(r)) = (&mut out, res) {
                for (k, v) in r {
                    o.insert(k, v);
                }
            }
            Json(out)
        }
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

/// Whether a reset snapshot exists (to enable the Restore button) + its size.
async fn api_data_snapshot(State(_s): State<Arc<AppState>>) -> Json<Value> {
    let path = crate::config::snapshot_path();
    match std::fs::read(&path) {
        Ok(bytes) => {
            let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let count = |k: &str| {
                v.get(k)
                    .and_then(|x| x.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0)
            };
            Json(
                json!({"exists": true, "sessions": count("sessions"), "samples": count("samples")}),
            )
        }
        Err(_) => Json(json!({"exists": false})),
    }
}

#[derive(Deserialize)]
struct SpeedMarkBody {
    speed: f64,
}

/// Record the speed the user has set on the treadmill (a timestamped marker we
/// can line up against the device's averaged speed readings).
async fn api_mark_speed(
    State(s): State<Arc<AppState>>,
    Json(b): Json<SpeedMarkBody>,
) -> Json<Value> {
    match s.db.insert_speed_mark(b.speed, &s.display_unit()) {
        Ok(id) => Json(json!({"ok": true, "id": id, "set_speed": b.speed})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

#[derive(Deserialize)]
struct DiagParams {
    /// Local date "YYYY-MM-DD"; defaults to today.
    date: Option<String>,
}

/// Downloadable diagnostic dump for one day (raw sessions/samples/rollups +
/// computed totals). Free to use — it's a support tool, not a data-export path.
async fn api_diag(State(s): State<Arc<AppState>>, Query(p): Query<DiagParams>) -> Response {
    let date = p
        .date
        .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());
    let mut diag = s.db.diag_day(&date).unwrap_or_else(|_| json!({}));
    if let Value::Object(ref mut m) = diag {
        m.insert("format".into(), json!("lifespan-sc110-diag"));
        m.insert("v".into(), json!(1));
        m.insert("app_version".into(), json!(env!("CARGO_PKG_VERSION")));
        m.insert(
            "generated_at".into(),
            json!(chrono::Local::now().timestamp()),
        );
        m.insert(
            "generated_at_iso".into(),
            json!(chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()),
        );
        m.insert("display_unit".into(), json!(s.display_unit()));
        // Samples the plausibility gate stripped a field from since launch.
        // Zero on a healthy decode; climbing during a hardware test means
        // the active driver's unit scale is wrong (PLAUSIBILITY GATE in
        // ble.rs; docs/drivers/README.md Step 4).
        m.insert("rejected_samples".into(), json!(s.rejected_samples()));
        m.insert("recent_frames".into(), json!(s.frames_snapshot()));
        m.insert(
            "speed_marks".into(),
            json!(s.db.recent_speed_marks(500).unwrap_or_default()),
        );
    }
    let body = serde_json::to_string_pretty(&diag).unwrap_or_default();
    let filename = format!("lifespan-sc110-diag-{date}.json");
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        body,
    )
        .into_response()
}

#[derive(Deserialize)]
struct ImportParams {
    #[serde(default = "default_mode")]
    mode: String,
}
fn default_mode() -> String {
    "merge".into()
}

async fn api_import(
    State(s): State<Arc<AppState>>,
    Query(p): Query<ImportParams>,
    body: axum::body::Bytes,
) -> Response {
    if p.mode != "merge" && p.mode != "replace" {
        return (StatusCode::BAD_REQUEST, "mode must be 'merge' or 'replace'").into_response();
    }
    let dump: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")).into_response(),
    };
    match s.db.import_dump(&dump, &p.mode) {
        Ok(result) => {
            s.invalidate_today(); // imported history may change today's totals
            let mut out = json!({"ok": true, "mode": p.mode});
            if let (Value::Object(ref mut o), Value::Object(r)) = (&mut out, result) {
                for (k, v) in r {
                    o.insert(k, v);
                }
            }
            Json(out).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

// ---- WebSocket -------------------------------------------------------------

async fn ws_handler(ws: WebSocketUpgrade, State(s): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| ws_loop(socket, s))
}

async fn ws_loop(mut socket: WebSocket, s: Arc<AppState>) {
    // Initial snapshot.
    let mut snap = s.snapshot();
    if let Value::Object(ref mut m) = snap {
        m.insert("type".into(), json!("snapshot"));
    }
    if socket.send(Message::Text(snap.to_string())).await.is_err() {
        return;
    }
    let mut rx = s.hub.subscribe();
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Ok(v) => {
                    if socket.send(Message::Text(v.to_string())).await.is_err() {
                        break;
                    }
                }
                Err(_) => continue, // lagged; keep going
            },
            incoming = socket.recv() => {
                if incoming.is_none() { break; }
            }
        }
    }
}

#[cfg(test)]
mod origin_tests {
    use super::is_allowed_origin;
    use axum::http::HeaderValue;

    fn allowed(origin: &str) -> bool {
        is_allowed_origin(&HeaderValue::from_str(origin).unwrap())
    }

    /// Every origin a Tauri webview can actually present, per platform and mode.
    ///
    /// These are not guesses — they are what `tauri-2.11`'s `tauri_protocol_url`
    /// and `PROXY_DEV_SERVER` produce:
    ///   * `windows || android` → `http(s)://tauri.localhost`
    ///   * everything else      → `tauri://localhost`
    ///   * desktop dev          → the devUrl verbatim (localhost)
    ///   * mobile dev           → `tauri://localhost`, because PROXY_DEV_SERVER
    ///     (`cfg!(all(dev, mobile))`) proxies a LAN devUrl through the custom
    ///     protocol rather than loading `http://192.168.x.x:5199` directly.
    ///
    /// If this list ever fails, the app gets 403 on every `/api` call and `/ws`
    /// refuses to upgrade — a total, silent outage of the UI on that platform.
    /// That is why it is a test and not a note in a document.
    #[test]
    fn every_platform_webview_origin_is_allowed() {
        for origin in [
            "tauri://localhost",       // macOS, iOS, Linux (prod); all mobile dev
            "http://tauri.localhost",  // Windows, Android (prod)
            "https://tauri.localhost", // ditto, with useHttpsScheme
            "http://localhost:5199",   // desktop dev server
            "http://127.0.0.1:5199",
            "http://[::1]:5199",
        ] {
            assert!(allowed(origin), "webview origin must be allowed: {origin}");
        }
    }

    #[test]
    fn a_hostile_page_is_rejected() {
        for origin in [
            "https://evil.example",
            "http://localhost.evil.example", // suffix trick
            "http://127.0.0.1.evil.example",
            "https://tauri.localhost.evil.example",
            "http://192.168.1.105:5199", // a LAN page is not the app
        ] {
            assert!(!allowed(origin), "must be rejected: {origin}");
        }
    }
}

#[derive(Deserialize)]
struct DiagnoseStart {
    seconds: u64,
}
async fn diagnose_start(State(s): State<Arc<AppState>>, Json(p): Json<DiagnoseStart>) -> Response {
    match s.diagnostics.start(p.seconds, "attached") {
        Ok(id) => {
            if let Some(v) = s
                .diagnostic_inventory
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                s.diagnostics.event(
                    "inventory_at_attach",
                    json!({"freshness":"last connection discovery; may be cached", "inventory":v}),
                );
            }
            s.diagnostics.event("attached",json!({"driver":s.driver(),"link_connected":s.connected.load(std::sync::atomic::Ordering::Relaxed),"display_unit":s.display_unit(),"initialization":"not observed unless connection starts during capture","initial_telemetry":s.last_state().as_ref().map(crate::app::state_dict)}));
            Json(json!({"capture_id":id,"engine_version":env!("CARGO_PKG_VERSION"),"schema":"trot.diagnostic.v1"})).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}
async fn diagnose_snapshot(
    State(s): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    match s.diagnostics.snapshot(&id) {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}
async fn diagnose_stop(
    State(s): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    match s.diagnostics.finish(&id, "client_finished") {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}
