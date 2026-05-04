//! firnflow-api — axum REST frontend for the firnflow core.
//!
//! The binary (`src/main.rs`) parses [`AppConfig`] from the
//! environment, builds an [`AppState`], and calls [`router`] to
//! mount the handlers. Tests reuse the same entry point by building
//! their own `AppState` (via `tests/common`) and driving [`router`]
//! through `tower::ServiceExt::oneshot`.

pub mod auth;
pub mod config;
pub mod error;
pub mod rate_limit;
pub mod state;

mod handlers;

use std::sync::Arc;

use axum::middleware::from_fn_with_state;
use axum::routing::{delete, get, post};
use axum::Router;
use tower::ServiceBuilder;

pub use config::AppConfig;
pub use error::ApiError;
pub use state::{build_state, AppState};

/// Build the axum router wired to the application state.
///
/// Endpoints split into four sub-routers along the threat-model
/// boundaries documented in `ISSUE_2.md`:
///
/// * **Public** — `GET /health`. No middleware, ever; k8s
///   liveness/readiness must work regardless of key configuration.
/// * **Metrics** — `GET /metrics`. Wrapped in
///   [`auth::require_metrics_token`], which short-circuits when no
///   `FIRNFLOW_METRICS_TOKEN` is configured (preserving the
///   pre-0.5.0 default-public behaviour).
/// * **Read/Write** — `upsert`, `query`, `list`, `warmup`. Stacks
///   `require_write` and the per-principal limiter inside a single
///   `ServiceBuilder` so the limiter sees the `Principal` extension
///   that auth attaches.
/// * **Admin** — `delete`, `index`, `fts-index`, `scalar-index`,
///   `compact`. Same shape as read/write but with `require_admin`.
///
/// The optional pre-auth IP limiter wraps the merged protected
/// router as the outermost layer — it intentionally runs before
/// auth so a credential-stuffing client cannot mint fresh buckets
/// per token.
pub fn router(state: AppState) -> Router {
    let auth_state = state.auth_state();
    let metrics = Arc::clone(&state.metrics);

    let public = Router::new().route("/health", get(handlers::health));

    let metrics_router = Router::new()
        .route("/metrics", get(handlers::metrics))
        .route_layer(from_fn_with_state(
            auth_state.clone(),
            auth::require_metrics_token,
        ));

    let principal_layer =
        rate_limit::build_principal_limiter(&state.rate_limit, Arc::clone(&metrics));

    let read = Router::new()
        .route("/ns/{namespace}/upsert", post(handlers::upsert))
        .route("/ns/{namespace}/query", post(handlers::query))
        .route("/ns/{namespace}/list", get(handlers::list))
        .route("/ns/{namespace}/warmup", post(handlers::warmup));
    // ServiceBuilder applies layers top-to-bottom: auth first
    // (attaches the Principal extension), principal limiter second
    // (reads it). `axum::Router::layer` then mounts the whole stack
    // in one call so the order is unambiguous. When the limiter is
    // disabled, the auth layer applies on its own.
    let read = match principal_layer.clone() {
        Some(limiter) => read.layer(
            ServiceBuilder::new()
                .layer(from_fn_with_state(auth_state.clone(), auth::require_write))
                .layer(limiter),
        ),
        None => read.layer(from_fn_with_state(auth_state.clone(), auth::require_write)),
    };

    let admin = Router::new()
        .route("/ns/{namespace}", delete(handlers::delete))
        .route("/ns/{namespace}/index", post(handlers::create_index))
        .route(
            "/ns/{namespace}/fts-index",
            post(handlers::create_fts_index),
        )
        .route(
            "/ns/{namespace}/scalar-index",
            post(handlers::create_scalar_index),
        )
        .route("/ns/{namespace}/compact", post(handlers::compact));
    let admin = match principal_layer {
        Some(limiter) => admin.layer(
            ServiceBuilder::new()
                .layer(from_fn_with_state(auth_state.clone(), auth::require_admin))
                .layer(limiter),
        ),
        None => admin.layer(from_fn_with_state(auth_state.clone(), auth::require_admin)),
    };

    let mut protected = read.merge(admin);

    if let Some(layer) = rate_limit::build_preauth_ip_limiter(&state.rate_limit, metrics) {
        protected = protected.layer(layer);
    }

    Router::new()
        .merge(public)
        .merge(metrics_router)
        .merge(protected)
        .with_state(state)
}
