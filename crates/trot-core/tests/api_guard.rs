//! End-to-end tests for the API's security guard and its bounds checks.
//!
//! This is the surface that decides who may read your activity data and who may
//! wipe it, so it gets exercised as real HTTP: a request goes through the actual
//! `Router` — middleware, extractors, handlers — rather than calling a handler
//! function directly. Everything here was previously untested.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt; // brings `oneshot`
use trot_core::app::AppState;
use trot_core::{api, db::Db};

const TOKEN: &str = "test-token-abc123";

fn app() -> axum::Router {
    let db = Arc::new(Db::open(":memory:").unwrap());
    let state = AppState::new(db, "km/h".into(), None, TOKEN.into());
    api::router(state)
}

/// Build a request; `Host` defaults to loopback so only the header under test varies.
fn req(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:1234")
        .body(Body::empty())
        .unwrap()
}

async fn status(r: Request<Body>) -> StatusCode {
    app().oneshot(r).await.unwrap().status()
}

// ---- token ----------------------------------------------------------------

#[tokio::test]
async fn writes_require_the_token() {
    // No token at all.
    assert_eq!(
        status(req("POST", "/api/disconnect")).await,
        StatusCode::FORBIDDEN
    );

    // A wrong token.
    let mut r = req("POST", "/api/disconnect");
    r.headers_mut()
        .insert("x-sc110-token", "nope".parse().unwrap());
    assert_eq!(status(r).await, StatusCode::FORBIDDEN);

    // The real token gets through.
    let mut r = req("POST", "/api/disconnect");
    r.headers_mut()
        .insert("x-sc110-token", TOKEN.parse().unwrap());
    assert_eq!(status(r).await, StatusCode::OK);
}

#[tokio::test]
async fn reads_do_not_require_the_token() {
    // Deliberate: a process running as you could read the SQLite file anyway,
    // so a token on reads would be theatre. Documented in the README.
    assert_eq!(status(req("GET", "/api/health")).await, StatusCode::OK);
}

// ---- Host header (DNS-rebinding) ------------------------------------------

#[tokio::test]
async fn non_loopback_host_is_rejected() {
    for host in [
        "evil.example",
        "127.0.0.1.evil.example", // suffix trick
        "localhost.evil.example",
        "0.0.0.0:1234",
        "192.168.1.10:1234", // a LAN address is not loopback
    ] {
        let r = Request::builder()
            .method("GET")
            .uri("/api/health")
            .header("host", host)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            status(r).await,
            StatusCode::FORBIDDEN,
            "host must be rejected: {host}"
        );
    }
}

#[tokio::test]
async fn loopback_hosts_are_accepted() {
    for host in [
        "127.0.0.1:1234",
        "localhost:1234",
        "[::1]:1234",
        "localhost",
    ] {
        let r = Request::builder()
            .method("GET")
            .uri("/api/health")
            .header("host", host)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            status(r).await,
            StatusCode::OK,
            "host must be allowed: {host}"
        );
    }
}

// ---- Origin (covers the /ws upgrade, which CORS does not) ------------------

#[tokio::test]
async fn a_hostile_origin_is_rejected_even_on_reads() {
    for origin in ["https://evil.example", "http://127.0.0.1.evil.example"] {
        let mut r = req("GET", "/api/state");
        r.headers_mut().insert("origin", origin.parse().unwrap());
        assert_eq!(
            status(r).await,
            StatusCode::FORBIDDEN,
            "origin must be rejected: {origin}"
        );
    }
}

#[tokio::test]
async fn the_app_and_dev_origins_are_accepted() {
    for origin in [
        "tauri://localhost",
        "http://tauri.localhost",
        "https://tauri.localhost",
        "http://localhost:5199",
    ] {
        let mut r = req("GET", "/api/state");
        r.headers_mut().insert("origin", origin.parse().unwrap());
        assert_eq!(
            status(r).await,
            StatusCode::OK,
            "origin must be allowed: {origin}"
        );
    }
}

#[tokio::test]
async fn no_origin_header_passes_through() {
    // The CLI and the app's own Rust calls send no Origin; they must not be
    // caught by a check aimed at browsers.
    assert_eq!(status(req("GET", "/api/state")).await, StatusCode::OK);
}

// ---- response hardening ----------------------------------------------------

#[tokio::test]
async fn responses_carry_nosniff() {
    let resp = app().oneshot(req("GET", "/api/health")).await.unwrap();
    assert_eq!(
        resp.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
}

// ---- bounds / guard rails --------------------------------------------------

#[tokio::test]
async fn analytics_rejects_an_absurd_bucket_count() {
    // 5 years at minute resolution is ~2.6M buckets — a cheap request that would
    // otherwise turn into a CPU and memory amplifier.
    let r = req(
        "GET",
        "/api/analytics?metric=steps&resolution=minute&range_days=1825",
    );
    assert_eq!(status(r).await, StatusCode::BAD_REQUEST);

    // The same range at a sane resolution is fine.
    let r = req(
        "GET",
        "/api/analytics?metric=steps&resolution=day&range_days=1825",
    );
    assert_eq!(status(r).await, StatusCode::OK);
}

#[tokio::test]
async fn analytics_validates_metric_and_resolution() {
    for uri in [
        "/api/analytics?metric=DROP+TABLE&resolution=hour&range_days=1",
        "/api/analytics?metric=steps&resolution=fortnight&range_days=1",
        "/api/analytics?metric=steps&resolution=hour&range_days=0",
    ] {
        assert_eq!(
            status(req("GET", uri)).await,
            StatusCode::BAD_REQUEST,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn reset_refuses_when_there_is_nothing_to_reset() {
    // Guards a real data-loss path: a second reset used to export the (now
    // empty) database over the snapshot the first reset had saved.
    let mut r = req("POST", "/api/data/reset");
    r.headers_mut()
        .insert("x-sc110-token", TOKEN.parse().unwrap());
    let resp = app().oneshot(r).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["ok"], false, "an empty database must not be 'reset'");
    assert!(
        v["error"].as_str().unwrap().contains("already empty"),
        "the error should say why: {v}"
    );
}

#[tokio::test]
async fn health_reports_the_engine_version() {
    // The desktop app ships the engine as a separate sidecar, so it can be older
    // than the app bundling it; clients surface both.
    let resp = app().oneshot(req("GET", "/api/health")).await.unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn unknown_routes_are_404_not_500() {
    assert_eq!(status(req("GET", "/api/nope")).await, StatusCode::NOT_FOUND);
}

/// `/api/state` carries the additive observability fields (`driver`,
/// tri-state `steps_supported`) WITHOUT moving any frozen field. Adding is
/// allowed; changing or removing is a breaking change (CONTRIBUTING.md:
/// /api + /ws is a public contract).
#[tokio::test]
async fn state_carries_the_additive_driver_and_steps_supported_fields() {
    let resp = app().oneshot(req("GET", "/api/state")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // The additive fields exist and start in their unknown states.
    assert!(
        v.as_object().unwrap().contains_key("driver"),
        "driver (additive, 0.3.x) missing from /api/state: {v}"
    );
    assert_eq!(v["driver"], serde_json::Value::Null);
    assert!(
        v.as_object().unwrap().contains_key("steps_supported"),
        "steps_supported (additive, 0.3.x) missing from /api/state: {v}"
    );
    assert_eq!(
        v["steps_supported"],
        serde_json::Value::Null,
        "steps_supported is TRI-STATE: null until a sample with steps is \
         observed, true thereafter, never false \
         (AppState::steps_supported_json in app.rs)"
    );

    // The frozen fields did not move.
    for key in [
        "connected",
        "paused",
        "connect_failed",
        "display_unit",
        "device_id",
        "state",
        "active_session_id",
        "today",
    ] {
        assert!(
            v.as_object().unwrap().contains_key(key),
            "frozen /api/state field {key:?} vanished — existing fields are \
             contract and must not move: {v}"
        );
    }
}

/// `/api/diag` exposes the plausibility gate's rejected-sample counter —
/// the loud half of the gate (its constants live in ble.rs).
#[tokio::test]
async fn diag_reports_the_gate_rejection_counter() {
    let resp = app().oneshot(req("GET", "/api/diag")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["rejected_samples"],
        serde_json::json!(0),
        "rejected_samples must be in /api/diag (and 0 on a fresh engine): a \
         silent plausibility gate is a maintainability loss — see the \
         PLAUSIBILITY GATE comment in ble.rs: {v}"
    );
}

// Diagnostic exports contain exact hardware bytes, so GET requires a token too.
#[tokio::test]
async fn diagnostics_are_authenticated_and_scoped() {
    let router = app();
    for (method, path) in [
        ("POST", "/api/diagnose"),
        ("GET", "/api/diagnose/unknown"),
        ("DELETE", "/api/diagnose/unknown"),
    ] {
        let response = router.clone().oneshot(req(method, path)).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let request = |seconds| {
        Request::builder()
            .method("POST")
            .uri("/api/diagnose")
            .header("host", "127.0.0.1")
            .header("content-type", "application/json")
            .header("x-sc110-token", TOKEN)
            .body(Body::from(format!("{{\"seconds\":{seconds}}}")))
            .unwrap()
    };
    assert_eq!(
        router.clone().oneshot(request(601)).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let response = router.clone().oneshot(request(60)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = body["capture_id"].as_str().unwrap();
    assert_eq!(
        router.clone().oneshot(request(60)).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let authenticated = |method, path: &str| {
        Request::builder()
            .method(method)
            .uri(path)
            .header("host", "127.0.0.1")
            .header("x-sc110-token", TOKEN)
            .body(Body::empty())
            .unwrap()
    };
    let response = router
        .clone()
        .oneshot(authenticated("GET", &format!("/api/diagnose/{id}")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["mode"], "attached");
    assert!(!String::from_utf8_lossy(&bytes).contains(TOKEN));
    assert!(body.get("sessions").is_none());
    assert_eq!(
        router
            .clone()
            .oneshot(authenticated("GET", "/api/diagnose/wrong"))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        router
            .oneshot(authenticated("DELETE", &format!("/api/diagnose/{id}")))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}
