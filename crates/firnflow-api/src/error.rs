//! API error type with axum `IntoResponse` integration.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use firnflow_core::FirnflowError;

/// Newtype over [`FirnflowError`] so we can implement axum's
/// `IntoResponse` without adding an axum dependency to `firnflow-core`.
pub struct ApiError(pub FirnflowError);

impl From<FirnflowError> for ApiError {
    fn from(e: FirnflowError) -> Self {
        Self(e)
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self.0 {
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
                // Full error goes to tracing; external callers get a
                // generic 500 so backend details do not leak.
                tracing::error!(error = %err, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };
        (status, Json(ErrorBody { error: msg })).into_response()
    }
}
