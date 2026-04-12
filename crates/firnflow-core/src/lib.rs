//! firnflow-core — tiered storage primitives for firnflow.
//!
//! This crate hosts the foyer-backed cache layer, the namespace
//! manager, and (in a later spike) the LanceDB wrapper. It is consumed
//! by `firnflow-api` and `firnflow-bench`.

#![warn(missing_docs)]

pub mod cache;
pub mod error;
pub mod manager;
pub mod metrics;
pub mod namespace;
pub mod query;
pub mod result;
pub mod service;

pub use error::FirnflowError;
pub use manager::NamespaceManager;
pub use metrics::CoreMetrics;
pub use namespace::NamespaceId;
pub use query::QueryRequest;
pub use result::{QueryResult, QueryResultSet};
pub use service::NamespaceService;
