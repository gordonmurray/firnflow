//! Error types for firnflow-core.

use thiserror::Error;

/// Top-level error type for firnflow-core operations.
#[derive(Debug, Error)]
pub enum FirnflowError {
    /// A cache backend operation failed (foyer, local NVMe device, …).
    #[error("cache backend error: {0}")]
    Cache(String),

    /// A storage backend operation failed (lancedb, object store, …).
    #[error("storage backend error: {0}")]
    Backend(String),

    /// An I/O error (disk, network, filesystem, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The requested namespace name is invalid.
    #[error("invalid namespace name: {0:?}")]
    InvalidNamespace(String),

    /// A request payload failed validation (wrong vector dimension,
    /// malformed query, …).
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// The requested operation or configuration is not supported by
    /// this build. Two callers today: a namespace-level operation
    /// whose Lance schema pre-dates a feature (the `/list` endpoint
    /// surfaces this as HTTP 501), and a startup-time storage URI
    /// using a scheme this build does not yet route (e.g. `gs://`
    /// before native GCS support ships).
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// A metrics registry or encoding operation failed.
    #[error("metrics error: {0}")]
    Metrics(String),
}
