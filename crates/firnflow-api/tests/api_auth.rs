//! Auth middleware integration tests.
//!
//! No MinIO required — every assertion fires before any handler
//! actually touches the namespace store. Tests that need to prove a
//! token *passed* auth aim at an invalid namespace name so the
//! handler short-circuits on `NamespaceId::new` and returns 400 rather
//! than racing the network: a 400 status proves the middleware did
//! not reject and the handler ran.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use firnflow_api::auth::{AuthConfig, Secret};
use firnflow_api::config::AppConfig;
use firnflow_api::rate_limit::RateLimitSettings;
use firnflow_api::router;
use firnflow_core::StorageRoot;
use serde_json::json;
use std::collections::HashMap;
use tower::ServiceExt;

mod common;
use common::test_state_offline_with_auth;

const WRITE_KEY: &str = "write-secret-token";
const ADMIN_KEY: &str = "admin-secret-token";
const METRICS_KEY: &str = "metrics-secret-token";

fn cfg(write: bool, admin: bool, metrics: bool) -> AuthConfig {
    AuthConfig::default()
        .with_write_key(write.then(|| Secret::new(WRITE_KEY)))
        .with_admin_key(admin.then(|| Secret::new(ADMIN_KEY)))
        .with_metrics_key(metrics.then(|| Secret::new(METRICS_KEY)))
}

fn query_body() -> Body {
    Body::from(json!({"vector": [0.0, 0.0], "k": 1}).to_string())
}

async fn build_app(auth: AuthConfig) -> (axum::Router, tempfile::TempDir) {
    let (state, tmp) = test_state_offline_with_auth(auth, RateLimitSettings::default()).await;
    (router(state), tmp)
}

fn query_request(token: Option<&str>) -> Request<Body> {
    let mut req = Request::builder()
        .method("POST")
        .uri("/ns/NOT_LOWERCASE/query")
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    req.body(query_body()).unwrap()
}

fn compact_request(token: Option<&str>, ns: &str) -> Request<Body> {
    let mut req = Request::builder()
        .method("POST")
        .uri(format!("/ns/{ns}/compact"));
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    req.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn health_open_without_auth() {
    // Even with both keys configured, /health stays open — k8s
    // liveness/readiness must not depend on auth configuration.
    let (app, _tmp) = build_app(cfg(true, true, false)).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn disabled_when_no_keys_set() {
    // No keys configured ⇒ `is_open()` ⇒ middleware short-circuits.
    // Handler reached, returns 400 on the bad namespace.
    let (app, _tmp) = build_app(AuthConfig::disabled()).await;
    let response = app.oneshot(query_request(None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn missing_header_returns_401() {
    let (app, _tmp) = build_app(cfg(true, true, false)).await;
    let response = app.oneshot(query_request(None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let www = response
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .expect("WWW-Authenticate header missing");
    assert_eq!(www, "Bearer realm=\"firnflow\"");
}

#[tokio::test]
async fn malformed_header_returns_401() {
    let (app, _tmp) = build_app(cfg(true, true, false)).await;
    let req = Request::builder()
        .method("POST")
        .uri("/ns/test/query")
        .header("content-type", "application/json")
        .header(header::AUTHORIZATION, "Token foo")
        .body(query_body())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_token_returns_401() {
    let (app, _tmp) = build_app(cfg(true, true, false)).await;
    let response = app
        .oneshot(query_request(Some("not-a-real-key")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn write_token_authorises_read_route() {
    let (app, _tmp) = build_app(cfg(true, true, false)).await;
    let response = app.oneshot(query_request(Some(WRITE_KEY))).await.unwrap();
    // Auth passed, handler validated namespace and returned 400.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn write_token_forbidden_on_admin_route_when_admin_key_set() {
    let (app, _tmp) = build_app(cfg(true, true, false)).await;
    let response = app
        .oneshot(compact_request(Some(WRITE_KEY), "test"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn single_key_authorises_admin_routes_when_admin_key_unset() {
    // Only write key configured ⇒ write key is promoted to admin
    // for admin routes (single-key fallback documented in §1).
    let (app, _tmp) = build_app(cfg(true, false, false)).await;
    let response = app
        .oneshot(compact_request(Some(WRITE_KEY), "NOT_LOWERCASE"))
        .await
        .unwrap();
    // Auth passed (single-key fallback). Handler returned 400 on the bad ns.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn admin_token_authorises_read_route() {
    let (app, _tmp) = build_app(cfg(true, true, false)).await;
    let response = app.oneshot(query_request(Some(ADMIN_KEY))).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn admin_token_authorises_admin_route() {
    let (app, _tmp) = build_app(cfg(true, true, false)).await;
    let response = app
        .oneshot(compact_request(Some(ADMIN_KEY), "NOT_LOWERCASE"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn metrics_open_when_token_unset() {
    let (app, _tmp) = build_app(cfg(true, true, false)).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn metrics_gated_when_token_set() {
    let (app, _tmp) = build_app(cfg(true, true, true)).await;

    // No token: 401
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Wrong token: 401
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .header(header::AUTHORIZATION, "Bearer wrong-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Correct token: 200
    let response = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .header(header::AUTHORIZATION, format!("Bearer {METRICS_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn secret_debug_does_not_leak_in_app_config() {
    let mut storage_options = HashMap::new();
    storage_options.insert(
        "aws_secret_access_key".into(),
        "UNIQUE_S3_SECRET_PLEASE_DO_NOT_LEAK".into(),
    );
    storage_options.insert(
        "aws_access_key_id".into(),
        "UNIQUE_S3_ACCESS_PLEASE_DO_NOT_LEAK".into(),
    );
    storage_options.insert("aws_endpoint".into(), "http://example:9000".into());

    let cfg = AppConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        storage_root: StorageRoot::s3_bucket("test").unwrap(),
        cache_memory_bytes: 0,
        cache_nvme_path: std::env::temp_dir(),
        cache_nvme_bytes: 0,
        storage_options,
        api_key: Some(Secret::new("UNIQUE_API_KEY_PLEASE_DO_NOT_LEAK")),
        admin_api_key: Some(Secret::new("UNIQUE_ADMIN_KEY_PLEASE_DO_NOT_LEAK")),
        metrics_token: Some(Secret::new("UNIQUE_METRICS_KEY_PLEASE_DO_NOT_LEAK")),
        rate_limit: RateLimitSettings::default(),
    };
    let dbg = format!("{:?}", cfg);
    assert!(
        !dbg.contains("UNIQUE_API_KEY"),
        "API key leaked in Debug: {dbg}"
    );
    assert!(
        !dbg.contains("UNIQUE_ADMIN_KEY"),
        "admin key leaked in Debug: {dbg}"
    );
    assert!(
        !dbg.contains("UNIQUE_METRICS_KEY"),
        "metrics token leaked in Debug: {dbg}"
    );
    assert!(
        !dbg.contains("UNIQUE_S3_SECRET"),
        "S3 secret access key leaked via storage_options Debug: {dbg}"
    );
    assert!(
        !dbg.contains("UNIQUE_S3_ACCESS"),
        "S3 access key id leaked via storage_options Debug: {dbg}"
    );
}

#[tokio::test]
async fn duplicate_keys_fail_startup_before_cache_setup() {
    // Pure config validation must run before FS / cache work, so a
    // duplicate-key misconfiguration is reported even when the
    // cache directory is unwritable. Point cache_nvme_path at a
    // location create_dir_all would fail on (a regular file's child
    // path) and assert the surfaced error names the duplicate keys,
    // not the FS failure.
    use firnflow_api::build_state;
    use std::path::PathBuf;

    let mut not_a_directory: PathBuf = std::env::temp_dir();
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let blocker = not_a_directory.join(format!("firnflow-not-a-dir-{unique}"));
    std::fs::write(&blocker, b"not a directory").expect("create blocker file in temp dir");
    not_a_directory = blocker.join("child");

    let cfg = AppConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        storage_root: StorageRoot::s3_bucket("firnflow-offline").unwrap(),
        cache_memory_bytes: 0,
        cache_nvme_path: not_a_directory,
        cache_nvme_bytes: 0,
        storage_options: HashMap::new(),
        api_key: Some(Secret::new("identical-token")),
        admin_api_key: Some(Secret::new("identical-token")),
        metrics_token: None,
        rate_limit: RateLimitSettings::default(),
    };

    let err = match build_state(&cfg).await {
        Ok(_) => panic!("build_state must reject duplicate keys"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("FIRNFLOW_API_KEY") && msg.contains("FIRNFLOW_ADMIN_API_KEY"),
        "expected duplicate-key error, got: {msg}"
    );
    assert!(
        !msg.contains("cache nvme directory"),
        "duplicate-key check ran AFTER the cache dir setup; cache error masked the auth error: {msg}"
    );

    // Cleanup the blocker file.
    let _ = std::fs::remove_file(&blocker);
}

#[tokio::test]
async fn auth_rejection_metric_counts_each_reason() {
    // Pin the §11 metric: each rejection class bumps a distinct
    // `reason` label. One missing-header request, one invalid-token
    // request, one forbidden-scope request — all three counters
    // should advance independently.
    let (state, _tmp) =
        test_state_offline_with_auth(cfg(true, true, false), RateLimitSettings::default()).await;
    let metrics = std::sync::Arc::clone(&state.metrics);
    let app = router(state);

    // missing
    let r = app.clone().oneshot(query_request(None)).await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    // invalid
    let r = app
        .clone()
        .oneshot(query_request(Some("nope")))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    // forbidden — write key on admin route, admin key configured
    let r = app
        .oneshot(compact_request(Some(WRITE_KEY), "test"))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);

    assert_eq!(metrics.auth_rejections_value("missing"), 1);
    assert_eq!(metrics.auth_rejections_value("invalid"), 1);
    assert_eq!(metrics.auth_rejections_value("forbidden"), 1);
    assert_eq!(metrics.auth_rejections_value("rate_limited"), 0);
}
