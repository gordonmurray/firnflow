//! Axum application state.

use std::sync::Arc;

use anyhow::Context;
use firnflow_core::cache::NamespaceCache;
use firnflow_core::{CoreMetrics, NamespaceManager, NamespaceService};

use crate::auth::{AuthConfig, AuthState};
use crate::config::AppConfig;
use crate::rate_limit::RateLimitSettings;

/// Shared state every handler receives via `axum::extract::State`.
///
/// Derives `Clone` because axum clones the state per-request to
/// hand it to extractors. Everything inside is already `Arc`-wrapped
/// so the clone is cheap.
#[derive(Clone)]
pub struct AppState {
    /// Service facade combining the cache and the namespace manager.
    /// Every cached read/write path goes through this.
    pub service: Arc<NamespaceService>,
    /// Direct manager handle for the one endpoint that intentionally
    /// bypasses the foyer cache (the `/list` path). Shares its
    /// underlying `NamespaceManager` with `service`; holding a
    /// separate `Arc` makes the architectural split explicit.
    pub manager: Arc<NamespaceManager>,
    /// Shared metrics registry; the `/metrics` handler encodes
    /// this into the Prometheus text format.
    pub metrics: Arc<CoreMetrics>,
    /// Auth + rate-limit configuration. `Arc` so middleware closures
    /// can share it without cloning the inner `Secret`s. Tests
    /// construct `AuthConfig::disabled()` (default-open) via
    /// [`AppState::new_with_defaults`] / the `tests/common` helper.
    pub auth: Arc<AuthConfig>,
    /// Rate-limit knobs separate from `auth` so the router can
    /// build governor layers without needing to mutate `auth`.
    pub rate_limit: RateLimitSettings,
}

impl AppState {
    /// Combine the auth config with metrics into the bundle the
    /// auth middleware extracts via `State<AuthState>`.
    pub fn auth_state(&self) -> AuthState {
        AuthState {
            config: Arc::clone(&self.auth),
            metrics: Arc::clone(&self.metrics),
        }
    }
}

/// Assemble `AppState` from an `AppConfig`. Pure config validation
/// (the duplicate-key check inside `build_auth_config`) runs first
/// so a misconfiguration is reported with no metrics, manager,
/// cache directory, or cache initialised — operators see the auth
/// error directly rather than having it masked by an unrelated
/// filesystem or cache setup failure further down. After validation
/// we build the metrics registry so it can be threaded into both
/// the foyer cache (hit/miss counters) and the namespace service
/// (query/write duration histograms, `s3_requests_total`), then
/// wire everything into one `NamespaceService`. Logs a single
/// `WARN` when neither auth key is set so an operator running
/// unauthenticated in production has a clear audit trail.
pub async fn build_state(cfg: &AppConfig) -> anyhow::Result<AppState> {
    // Validate before any side effects (metrics, FS, cache). A pure
    // config error must surface before unrelated infrastructure
    // failures could mask it.
    let auth = build_auth_config(cfg)?;

    let metrics =
        Arc::new(CoreMetrics::new().map_err(|e| anyhow::anyhow!("build metrics registry: {e}"))?);

    let manager = Arc::new(NamespaceManager::new(
        cfg.storage_root.clone(),
        cfg.storage_options.clone(),
        Arc::clone(&metrics),
    ));

    std::fs::create_dir_all(&cfg.cache_nvme_path).with_context(|| {
        format!(
            "creating cache nvme directory {}",
            cfg.cache_nvme_path.display()
        )
    })?;

    let cache = Arc::new(
        NamespaceCache::new(
            cfg.cache_memory_bytes,
            &cfg.cache_nvme_path,
            cfg.cache_nvme_bytes,
            Arc::clone(&metrics),
        )
        .await
        .map_err(|e| anyhow::anyhow!("build namespace cache: {e}"))?,
    );

    let service = Arc::new(NamespaceService::new(
        Arc::clone(&manager),
        cache,
        Arc::clone(&metrics),
    ));

    Ok(AppState {
        service,
        manager,
        metrics,
        auth: Arc::new(auth),
        rate_limit: cfg.rate_limit.clone(),
    })
}

/// Assemble the [`AuthConfig`] and apply startup-time validation.
///
/// Refuses to proceed when both the read/write and admin keys are
/// configured to the same value: `principal_for` checks the admin
/// key first, so identical bytes would silently classify every
/// request as admin and collapse the scope split. The operator
/// either wanted distinct keys (and made a copy/paste mistake) or
/// wanted single-tier behaviour (in which case `FIRNFLOW_ADMIN_API_KEY`
/// should be unset to engage the documented single-key fallback).
/// Either way, refusing startup is the safer answer.
fn build_auth_config(cfg: &AppConfig) -> anyhow::Result<AuthConfig> {
    let auth = AuthConfig::default()
        .with_write_key(cfg.api_key.clone())
        .with_admin_key(cfg.admin_api_key.clone())
        .with_metrics_key(cfg.metrics_token.clone())
        .with_trust_proxy_headers(cfg.rate_limit.trust_proxy_headers);

    if auth.duplicate_write_and_admin_keys() {
        anyhow::bail!(
            "FIRNFLOW_API_KEY and FIRNFLOW_ADMIN_API_KEY are configured to \
             the same value; the scope split would silently collapse and \
             every authenticated request would be treated as admin. Either \
             set FIRNFLOW_ADMIN_API_KEY to a distinct value, or unset it \
             to use the documented single-key fallback."
        );
    }

    if auth.is_open() {
        tracing::warn!(
            "firnflow API is running without authentication; \
             set FIRNFLOW_API_KEY before exposing this service"
        );
    } else {
        tracing::info!(
            admin_key_configured = cfg.admin_api_key.is_some(),
            metrics_token_configured = cfg.metrics_token.is_some(),
            trust_proxy_headers = cfg.rate_limit.trust_proxy_headers,
            "auth enabled"
        );
        if cfg.api_key.is_some() && cfg.admin_api_key.is_none() {
            tracing::info!(
                "single-key fallback active: FIRNFLOW_API_KEY also \
                 authorises admin routes (set FIRNFLOW_ADMIN_API_KEY \
                 to lock destructive ops behind a separate key)"
            );
        }
    }
    Ok(auth)
}
