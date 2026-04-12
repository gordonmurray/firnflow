//! Query request types.
//!
//! Request payloads are kept in their own module so that `result.rs`
//! stays focused on the response side. Both modules share the same
//! serde derives and are what the axum handlers will parse straight
//! from request bodies once slice 1c lands.

use serde::{Deserialize, Serialize};

/// Parameters of a vector nearest-neighbour query.
///
/// The `vector` must match the namespace's configured dimension;
/// validation happens at the manager boundary. `k` bounds the
/// number of results the caller wants back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryRequest {
    /// The query vector.
    pub vector: Vec<f32>,
    /// Maximum number of results to return.
    pub k: usize,
}
