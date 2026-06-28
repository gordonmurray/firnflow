//! Query result set types.
//!
//! These types model what a vector / FTS / hybrid search returns and
//! are the payload the cache layer serialises and stores as bytes.
//! The design is deliberately simple so that it exercises a realistic
//! shape for the serialisation benchmark without committing
//! prematurely to features (metadata, highlighting, etc.) that will
//! follow once the API layer is wired up.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::vector::VectorKind;

/// A single search hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    /// Stable row id from the underlying Lance table.
    pub id: u64,
    /// Similarity score — the metric (cosine, L2, BM25, hybrid
    /// relevance) is determined by the query type; the cache does
    /// not interpret it.
    pub score: f32,
    /// The stored vector that produced this hit. Width is fixed per
    /// namespace; the cache serialises the literal bytes. `None` when
    /// the caller opted out via `include_vector: false`, and always
    /// `None` for multivector hits (the bag of sub-vectors is
    /// hundreds of KB and is intentionally not echoed back).
    ///
    /// Plain `#[serde(default)]` on purpose — `skip_serializing_if`
    /// would break the positional bincode encoding this type round-
    /// trips through in the result cache. JSON renders `None` as
    /// `null`.
    #[serde(default)]
    pub vector: Option<Vec<f32>>,
    /// The stored text for this hit, if the namespace has a text
    /// column and the row was upserted with text.
    #[serde(default)]
    pub text: Option<String>,
    /// Server-side timestamp (microseconds since Unix epoch) of the
    /// most recent write to the row — the same `_ingested_at` value
    /// `/list` returns; a re-upsert of the same `id` advances it.
    /// `None` for namespaces created before the column existed.
    #[serde(default)]
    pub ingested_at_micros: Option<i64>,
    /// User-defined scalar attributes stored on the row.
    #[serde(default)]
    pub attributes: serde_json::Map<String, Value>,
}

/// A full query response: ranked hits plus an opaque tracing id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResultSet {
    /// Opaque query identifier for tracing/debugging. Does *not*
    /// participate in the cache key — that is derived from the query
    /// parameters, not from this field.
    pub query_id: String,
    /// Search hits, already ranked by the underlying engine.
    pub results: Vec<QueryResult>,
}

/// One value-count bucket in a facet response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FacetBucket {
    /// JSON scalar value for the bucket. Missing/null values are
    /// represented as JSON `null`.
    pub value: Value,
    /// Number of rows in the filtered set with this value.
    pub count: u64,
}

/// Buckets for one requested facet field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FacetField {
    /// Column name.
    pub field: String,
    /// Buckets sorted count-desc, then value-asc.
    pub buckets: Vec<FacetBucket>,
    /// True when more distinct values existed than the requested
    /// per-field `top`.
    pub truncated: bool,
}

/// Complete facet response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FacetResultSet {
    /// One entry per requested field.
    pub facets: Vec<FacetField>,
}

/// Sort order for the `list` endpoint. Descending returns newest
/// rows first and is the intended default for "recent content" flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListOrder {
    /// Oldest first.
    Asc,
    /// Newest first.
    Desc,
}

/// A single row returned by the list endpoint. Deliberately distinct
/// from [`QueryResult`] — there is no score (the endpoint does not
/// rank by similarity) and there is an ingest timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListRow {
    /// Stable row id from the underlying Lance table.
    pub id: u64,
    /// The stored vector for this row. Width matches the
    /// namespace's dimension.
    pub vector: Vec<f32>,
    /// The stored text for this row, if any.
    #[serde(default)]
    pub text: Option<String>,
    /// Server-side timestamp (microseconds since Unix epoch) of the
    /// most recent write to this row. Re-upserting the same `id`
    /// replaces the row and advances this value, so it reflects the
    /// latest write rather than the first insert.
    pub ingested_at_micros: i64,
    /// User-defined scalar attributes stored on the row.
    #[serde(default)]
    pub attributes: serde_json::Map<String, Value>,
}

/// A page of list results plus an opaque cursor for the next page.
/// `next_cursor` is `None` when the server returned the final page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListPage {
    /// Rows in the requested order.
    pub rows: Vec<ListRow>,
    /// Opaque cursor to pass as `?cursor=` on the next call, or
    /// `None` if no further rows are available in the chosen order.
    pub next_cursor: Option<String>,
}

/// Operational metadata for a namespace, returned by
/// `GET /ns/{namespace}`.
///
/// Read directly from the Lance table — it is namespace state, not a
/// query result, so it is never cached.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamespaceInfo {
    /// Namespace name.
    pub namespace: String,
    /// Vector representation: `"single"` or `"multivector"`.
    pub kind: VectorKind,
    /// Vector dimension. For multivector namespaces this is the inner
    /// sub-vector width.
    pub vector_dim: usize,
    /// Live row count (`Table::count_rows`). Upsert is keyed by `id`
    /// (latest-write-wins), so re-sending an existing id replaces the
    /// row rather than adding one; this is the count of distinct live
    /// ids.
    pub row_count: usize,
    /// Number of Lance data fragments. A growing count is the signal
    /// to `POST /ns/{namespace}/compact`.
    pub fragment_count: usize,
    /// Whether a vector index (the IVF_PQ / HNSW family) is built.
    pub has_vector_index: bool,
    /// Whether a BM25 full-text index is built on the `text` column.
    pub has_fts_index: bool,
    /// Whether a scalar index (BTree / bitmap / label-list) is built.
    pub has_scalar_index: bool,
    /// Current Lance table version. Advances on every commit; this is
    /// the value the result cache derives its generation from.
    pub table_version: u64,
}
