//! Query result set types.
//!
//! These types model what a vector / FTS / hybrid search returns and
//! are the payload the cache layer serialises and stores as bytes.
//! The design is deliberately simple so that it exercises a realistic
//! shape for the spike-3 serialisation benchmark without committing
//! prematurely to features (metadata, highlighting, etc.) that will
//! follow once the API layer is wired up.

use serde::{Deserialize, Serialize};

/// A single search hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    /// Stable row id from the underlying Lance table.
    pub id: u64,
    /// Similarity score — the metric (cosine, L2, BM25, …) is
    /// determined by the query; the cache does not interpret it.
    pub score: f32,
    /// The stored vector that produced this hit. Width is fixed per
    /// namespace; the cache serialises the literal bytes.
    pub vector: Vec<f32>,
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
