//! firnflow-api — axum REST frontend for the firnflow core.
//!
//! The binary (`src/main.rs`) parses `AppConfig` from the environment,
//! builds an `AppState`, and calls [`router`] to mount the handlers.
//! Tests reuse the same entry point by building their own
//! `AppState` and driving [`router`] via
//! `tower::ServiceExt::oneshot`.

pub mod config;
pub mod error;
pub mod state;

mod handlers;

use axum::routing::{delete, get, post};
use axum::Router;

pub use config::AppConfig;
pub use error::ApiError;
pub use state::{build_state, AppState};

/// Build the axum router wired to the application state.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(handlers::health))
        .route("/metrics", get(handlers::metrics))
        .route("/ns/{namespace}", delete(handlers::delete))
        .route("/ns/{namespace}/upsert", post(handlers::upsert))
        .route("/ns/{namespace}/query", post(handlers::query))
        .route("/ns/{namespace}/warmup", post(handlers::warmup))
        .with_state(state)
}
