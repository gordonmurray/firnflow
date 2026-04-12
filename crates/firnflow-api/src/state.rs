//! Axum application state.

use std::sync::Arc;

use anyhow::Context;
use firnflow_core::cache::NamespaceCache;
use firnflow_core::{CoreMetrics, NamespaceManager, NamespaceService};

use crate::config::AppConfig;

/// Shared state every handler receives via `axum::extract::State`.
///
/// Derives `Clone` because axum clones the state per-request to
/// hand it to extractors. Everything inside is already `Arc`-wrapped
/// so the clone is cheap.
#[derive(Clone)]
pub struct AppState {
    /// Service facade combining the cache and the namespace manager.
    pub service: Arc<NamespaceService>,
    /// Shared metrics registry; the `/metrics` handler encodes
    /// this into the Prometheus text format.
    pub metrics: Arc<CoreMetrics>,
}

/// Assemble `AppState` from an `AppConfig`. Builds the metrics
/// registry first so it can be threaded into both the foyer cache
/// (which records hit/miss counters) and the namespace service
/// (which records query/write duration histograms and the
/// `s3_requests_total` counter), then wires everything into one
/// `NamespaceService`.
pub async fn build_state(cfg: &AppConfig) -> anyhow::Result<AppState> {
    let metrics =
        Arc::new(CoreMetrics::new().map_err(|e| anyhow::anyhow!("build metrics registry: {e}"))?);

    let manager = Arc::new(NamespaceManager::new(
        cfg.bucket.clone(),
        cfg.storage_options.clone(),
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

    let service = Arc::new(NamespaceService::new(manager, cache, Arc::clone(&metrics)));
    Ok(AppState { service, metrics })
}
