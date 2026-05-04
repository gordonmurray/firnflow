//! API error type with axum `IntoResponse` integration.
//!
//! Promoted from a newtype-over-`FirnflowError` to an enum in 0.5.0.
//! API-layer-only conditions — bearer-token rejection, scope
//! mismatch, rate-limit shedding — are real variants now rather than
//! synthetic `FirnflowError`s squeezed through a wrapper.
//! `From<FirnflowError>` is preserved so handlers continue to use
//! `?` on core calls.

use std::time::Duration;

use axum::http::header;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use firnflow_core::FirnflowError;

/// API error variants. `Core` wraps a `firnflow-core` error and
/// inherits its existing 4xx/5xx mapping. `Unauthorized`,
/// `Forbidden`, and `RateLimited` are API-layer-only.
pub enum ApiError {
    Core(FirnflowError),
    Unauthorized,
    Forbidden,
    RateLimited(Duration),
}

impl From<FirnflowError> for ApiError {
    fn from(e: FirnflowError) -> Self {
        Self::Core(e)
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::Core(err) => core_response(err),
            ApiError::Unauthorized => {
                let mut response = (
                    StatusCode::UNAUTHORIZED,
                    Json(ErrorBody {
                        error: "unauthorized".into(),
                    }),
                )
                    .into_response();
                response.headers_mut().insert(
                    header::WWW_AUTHENTICATE,
                    HeaderValue::from_static("Bearer realm=\"firnflow\""),
                );
                response
            }
            ApiError::Forbidden => (
                StatusCode::FORBIDDEN,
                Json(ErrorBody {
                    error: "forbidden".into(),
                }),
            )
                .into_response(),
            ApiError::RateLimited(wait) => {
                let secs = wait.as_secs().max(1);
                let mut response = (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(ErrorBody {
                        error: "rate limited".into(),
                    }),
                )
                    .into_response();
                if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                    response.headers_mut().insert(header::RETRY_AFTER, v);
                }
                response
            }
        }
    }
}

fn core_response(err: FirnflowError) -> Response {
    let (status, msg) = match err {
        FirnflowError::InvalidNamespace(name) => (
            StatusCode::BAD_REQUEST,
            format!("invalid namespace: {name}"),
        ),
        FirnflowError::InvalidRequest(msg) => {
            (StatusCode::BAD_REQUEST, format!("invalid request: {msg}"))
        }
        FirnflowError::Unsupported(msg) => {
            (StatusCode::NOT_IMPLEMENTED, format!("not supported: {msg}"))
        }
        err @ (FirnflowError::Backend(_)
        | FirnflowError::Cache(_)
        | FirnflowError::Io(_)
        | FirnflowError::Metrics(_)) => {
            tracing::error!(error = %err, "internal error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal server error".to_string(),
            )
        }
    };
    (status, Json(ErrorBody { error: msg })).into_response()
}
