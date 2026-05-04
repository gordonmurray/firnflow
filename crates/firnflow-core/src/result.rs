//! Query result set types.
//!
//! These types model what a vector / FTS / hybrid search returns and
//! are the payload the cache layer serialises and stores as bytes.
//! The design is deliberately simple so that it exercises a realistic
//! shape for the serialisation benchmark without committing
//! prematurely to features (metadata, highlighting, etc.) that will
//! follow once the API layer is wired up.

use serde::{Deserialize, Serialize};

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
    /// namespace; the cache serialises the literal bytes.
    pub vector: Vec<f32>,
    /// The stored text for this hit, if the namespace has a text
    /// column and the row was upserted with text.
    #[serde(default)]
    pub text: Option<String>,
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
    /// Server-side timestamp (microseconds since Unix epoch) the
    /// row was first written. Immutable for the life of the row —
    /// Lance appends never rewrite it.
    pub ingested_at_micros: i64,
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
