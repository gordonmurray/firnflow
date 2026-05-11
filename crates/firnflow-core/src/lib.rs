//! firnflow-core — tiered storage primitives for firnflow.
//!
//! This crate hosts the foyer-backed cache layer, the namespace
//! manager, and the LanceDB wrapper. It is consumed by
//! `firnflow-api` and `firnflow-bench`.

#![warn(missing_docs)]

pub mod cache;
pub mod error;
pub mod manager;
pub mod metrics;
pub mod namespace;
pub mod query;
pub mod result;
pub mod service;
pub mod storage_root;

pub use error::FirnflowError;
pub use manager::{
    decode_list_cursor, encode_list_cursor, CompactResult, NamespaceManager, UpsertRow,
    LIST_MAX_LIMIT,
};
pub use metrics::CoreMetrics;
pub use namespace::NamespaceId;
pub use query::{IndexRequest, QueryRequest};
pub use result::{ListOrder, ListPage, ListRow, QueryResult, QueryResultSet};
pub use service::NamespaceService;
pub use storage_root::{Scheme, StorageRoot};
