//! Rate-limit middleware integration tests.
//!
//! Like `api_auth.rs`, no MinIO required: tests that need a request
//! to *pass* both auth and the limiter aim at an invalid namespace so
//! the handler returns 400 before touching any backend. The limiter
//! uses real wall-clock time (governor's `QuantaInstant`); each
//! oneshot call returns in microseconds, so the bucket cannot
//! replenish between consecutive calls inside a single test.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use firnflow_api::auth::{AuthConfig, Secret};
use firnflow_api::rate_limit::RateLimitSettings;
use firnflow_api::router;
use serde_json::json;
use tower::ServiceExt;

mod common;
use common::test_state_offline_with_auth;

const WRITE_KEY: &str = "write-secret-token";
const ADMIN_KEY: &str = "admin-secret-token";

fn auth_with_keys() -> AuthConfig {
    AuthConfig::default()
        .with_write_key(Some(Secret::new(WRITE_KEY)))
        .with_admin_key(Some(Secret::new(ADMIN_KEY)))
}

async fn build_app(
    auth: AuthConfig,
    rate_limit: RateLimitSettings,
) -> (axum::Router, tempfile::TempDir) {
    let (state, tmp) = test_state_offline_with_auth(auth, rate_limit).await;
    (router(state), tmp)
}

fn query_request(token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/ns/NOT_LOWERCASE/query")
        .header("content-type", "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(
            json!({"vector": [0.0, 0.0], "k": 1}).to_string(),
        ))
        .unwrap()
}

#[tokio::test]
async fn burst_then_429() {
    // burst=2 ⇒ 2 immediate calls go through; the 3rd is rate-limited.
    let rate_limit = RateLimitSettings {
        per_principal_rps: Some(1),
        burst_size: Some(2),
        preauth_ip_rps: None,
        trust_proxy_headers: false,
    };
    let (app, _tmp) = build_app(auth_with_keys(), rate_limit).await;

    let r = app.clone().oneshot(query_request(WRITE_KEY)).await.unwrap();
    assert_eq!(
        r.status(),
        StatusCode::BAD_REQUEST,
        "1st request should reach handler"
    );

    let r = app.clone().oneshot(query_request(WRITE_KEY)).await.unwrap();
    assert_eq!(
        r.status(),
        StatusCode::BAD_REQUEST,
        "2nd request should reach handler"
    );

    let r = app.clone().oneshot(query_request(WRITE_KEY)).await.unwrap();
    assert_eq!(
        r.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "3rd request should be rate-limited"
    );
    assert!(
        r.headers().contains_key(header::RETRY_AFTER),
        "Retry-After header missing on 429"
    );
}

#[tokio::test]
async fn health_exempt() {
    let rate_limit = RateLimitSettings {
        per_principal_rps: Some(1),
        burst_size: Some(1),
        preauth_ip_rps: Some(1),
        trust_proxy_headers: false,
    };
    let (app, _tmp) = build_app(auth_with_keys(), rate_limit).await;

    for i in 0..20 {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::OK,
            "/health rate-limited at iteration {i}"
        );
    }
}

#[tokio::test]
async fn metrics_exempt() {
    let rate_limit = RateLimitSettings {
        per_principal_rps: Some(1),
        burst_size: Some(1),
        preauth_ip_rps: Some(1),
        trust_proxy_headers: false,
    };
    let (app, _tmp) = build_app(auth_with_keys(), rate_limit).await;

    for i in 0..20 {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::OK,
            "/metrics rate-limited at iteration {i}"
        );
    }
}

#[tokio::test]
async fn per_principal_isolation() {
    // burst=1: each principal has its own bucket. Admin exhausts theirs;
    // write is unaffected.
    let rate_limit = RateLimitSettings {
        per_principal_rps: Some(1),
        burst_size: Some(1),
        preauth_ip_rps: None,
        trust_proxy_headers: false,
    };
    let (app, _tmp) = build_app(auth_with_keys(), rate_limit).await;

    // Admin: first call passes (bucket 1→0), second is 429.
    let r = app.clone().oneshot(query_request(ADMIN_KEY)).await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = app.clone().oneshot(query_request(ADMIN_KEY)).await.unwrap();
    assert_eq!(
        r.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "admin bucket should be exhausted"
    );

    // Write: separate bucket, first call passes.
    let r = app.clone().oneshot(query_request(WRITE_KEY)).await.unwrap();
    assert_eq!(
        r.status(),
        StatusCode::BAD_REQUEST,
        "write principal should have its own bucket"
    );
}

#[tokio::test]
async fn preauth_ip_limiter_caps_unauthenticated_attempts() {
    // burst=2 on the pre-auth IP limiter ⇒ 2 invalid-token attempts
    // get the expected 401 from auth, and the 3rd is shed by the IP
    // limiter before auth even runs.
    let rate_limit = RateLimitSettings {
        per_principal_rps: None,
        burst_size: Some(2),
        preauth_ip_rps: Some(1),
        trust_proxy_headers: false,
    };
    let (app, _tmp) = build_app(auth_with_keys(), rate_limit).await;

    let bad_request = || -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/ns/test/query")
            .header("content-type", "application/json")
            .header(header::AUTHORIZATION, "Bearer not-a-real-key")
            .body(Body::from(
                json!({"vector": [0.0, 0.0], "k": 1}).to_string(),
            ))
            .unwrap()
    };

    let r = app.clone().oneshot(bad_request()).await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = app.clone().oneshot(bad_request()).await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = app.clone().oneshot(bad_request()).await.unwrap();
    assert_eq!(
        r.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "preauth IP limiter should fire on the 3rd attempt"
    );
}
