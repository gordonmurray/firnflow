//! firnflow-core — tiered storage primitives for firnflow.
//!
//! This crate hosts the foyer-backed cache layer, the namespace
//! manager, and the LanceDB wrapper. It is consumed by
//! `firnflow-api` and `firnflow-bench`.

#![warn(missing_docs)]

pub mod attributes;
pub mod cache;
pub mod error;
pub mod filter;
pub mod manager;
pub mod metrics;
pub mod namespace;
pub mod object_cache;
pub mod query;
pub mod result;
pub mod service;
pub mod storage_root;
pub mod vector;

pub use attributes::{
    AttributeColumn, AttributeInput, AttributeMap, AttributeType, AttributeValue,
    MAX_ATTRIBUTE_COLUMNS, MAX_ATTRIBUTE_NAME_LEN, validate_attribute_declaration,
    validate_attribute_name,
};
pub use error::FirnflowError;
pub use filter::{FilterCacheability, classify_filter};
pub use manager::{
    CompactResult, LIST_MAX_LIMIT, NamespaceManager, UpsertRow, decode_list_cursor,
    encode_list_cursor, validate_arrow_import_schema,
};
pub use metrics::CoreMetrics;
pub use namespace::NamespaceId;
pub use query::{
    DEFAULT_SEMANTIC_MIN_SIMILARITY, IndexRequest, QueryRequest, SemanticCacheRequest,
    effective_semantic_threshold, validate_ivf_pq_options, validate_semantic_cache_request,
};
pub use result::{ListOrder, ListPage, ListRow, NamespaceInfo, QueryResult, QueryResultSet};
pub use service::{NamespaceService, QueryCacheSource, QueryOutcome};
pub use storage_root::{Scheme, StorageRoot, resolve_s3_region};
pub use vector::VectorKind;
