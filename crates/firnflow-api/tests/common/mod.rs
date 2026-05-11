//! Shared helpers for integration tests.
//!
//! Adding a new field to `AppState` should change exactly one line
//! here, not ten — this module is the single point that constructs
//! `AppState` for tests so the auth + rate-limit fields stay in
//! sync without per-test churn.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use firnflow_api::auth::{AuthConfig, Secret};
use firnflow_api::config::AppConfig;
use firnflow_api::rate_limit::RateLimitSettings;
use firnflow_api::AppState;
use firnflow_core::cache::NamespaceCache;
use firnflow_core::metrics::test_metrics;
use firnflow_core::{NamespaceManager, NamespaceService, StorageRoot};

/// Read an env var with a default. Used to override MinIO defaults
/// in CI where the bucket and endpoint may differ from local dev.
pub fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Generate a unique namespace name. Uses nanoseconds since epoch
/// so concurrent tests do not share state on MinIO.
pub fn unique_namespace(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{nanos}")
}

/// MinIO `object_store` options for the local docker-compose stack.
pub fn minio_options() -> HashMap<String, String> {
    HashMap::from([
        (
            "aws_access_key_id".into(),
            env_or("FIRNFLOW_S3_ACCESS_KEY", "minioadmin"),
        ),
        (
            "aws_secret_access_key".into(),
            env_or("FIRNFLOW_S3_SECRET_KEY", "minioadmin"),
        ),
        (
            "aws_endpoint".into(),
            env_or("FIRNFLOW_S3_ENDPOINT", "http://127.0.0.1:9000"),
        ),
        ("aws_region".into(), "us-east-1".into()),
        ("allow_http".into(), "true".into()),
        ("aws_virtual_hosted_style_request".into(), "false".into()),
    ])
}

/// Build a default-open `AppState` against the local MinIO stack.
/// Returns the temp dir alongside so the caller can keep it alive
/// for the duration of the test (foyer NVMe tier lives here).
pub async fn test_state() -> (AppState, tempfile::TempDir) {
    test_state_with_auth(AuthConfig::disabled(), RateLimitSettings::default()).await
}

/// Build an `AppState` against MinIO with the provided auth + rate
/// limit settings.
pub async fn test_state_with_auth(
    auth: AuthConfig,
    rate_limit: RateLimitSettings,
) -> (AppState, tempfile::TempDir) {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let storage_root = StorageRoot::s3_bucket(&bucket).expect("test bucket name");
    let tmp = tempfile::tempdir().unwrap();
    let metrics = test_metrics();

    let manager = Arc::new(NamespaceManager::new(
        storage_root,
        minio_options(),
        Arc::clone(&metrics),
    ));
    let cache = Arc::new(
        NamespaceCache::new(
            16 * 1024 * 1024,
            tmp.path(),
            64 * 1024 * 1024,
            Arc::clone(&metrics),
        )
        .await
        .unwrap(),
    );
    let service = Arc::new(NamespaceService::new(
        Arc::clone(&manager),
        cache,
        Arc::clone(&metrics),
    ));
    let state = AppState {
        service,
        manager,
        metrics,
        auth: Arc::new(auth),
        rate_limit,
    };
    (state, tmp)
}

/// Build an `AppState` with **no** S3 backend touch — purely
/// in-process, suitable for tests whose every assertion fires
/// before reaching a handler (auth rejection, rate-limit shedding,
/// /health). Skips MinIO entirely so these tests can run without
/// docker compose up.
pub async fn test_state_offline() -> (AppState, tempfile::TempDir) {
    test_state_offline_with_auth(AuthConfig::disabled(), RateLimitSettings::default()).await
}

pub async fn test_state_offline_with_auth(
    auth: AuthConfig,
    rate_limit: RateLimitSettings,
) -> (AppState, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let metrics = test_metrics();
    // Bucket name is irrelevant — the manager is never touched
    // because the auth/limiter middleware short-circuits before any
    // handler runs. Use an obviously-fake hostname so any accidental
    // touch fails loudly rather than racing with a real MinIO.
    let manager = Arc::new(NamespaceManager::new(
        StorageRoot::s3_bucket("firnflow-offline").unwrap(),
        HashMap::from([
            (
                "aws_endpoint".into(),
                "http://127.0.0.1:1".into(), // refuses connections
            ),
            ("aws_region".into(), "us-east-1".into()),
            ("allow_http".into(), "true".into()),
            ("aws_virtual_hosted_style_request".into(), "false".into()),
            ("aws_access_key_id".into(), "x".into()),
            ("aws_secret_access_key".into(), "x".into()),
        ]),
        Arc::clone(&metrics),
    ));
    let cache = Arc::new(
        NamespaceCache::new(
            4 * 1024 * 1024,
            tmp.path(),
            16 * 1024 * 1024,
            Arc::clone(&metrics),
        )
        .await
        .unwrap(),
    );
    let service = Arc::new(NamespaceService::new(
        Arc::clone(&manager),
        cache,
        Arc::clone(&metrics),
    ));
    let state = AppState {
        service,
        manager,
        metrics,
        auth: Arc::new(auth),
        rate_limit,
    };
    (state, tmp)
}

/// Convenience: a minimal `AppConfig` used to verify env-var
/// parsing in the auth tests. Not used by integration tests that
/// drive the router directly.
pub fn dummy_config() -> AppConfig {
    AppConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        storage_root: StorageRoot::s3_bucket("firnflow-offline").unwrap(),
        cache_memory_bytes: 4 * 1024 * 1024,
        cache_nvme_path: std::env::temp_dir(),
        cache_nvme_bytes: 16 * 1024 * 1024,
        storage_options: HashMap::new(),
        api_key: None,
        admin_api_key: None,
        metrics_token: None,
        rate_limit: RateLimitSettings::default(),
    }
}

/// Construct a `Secret` from a string literal in tests.
pub fn secret(s: &str) -> Secret {
    Secret::new(s)
}
