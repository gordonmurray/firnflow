//! Cache key types.

use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::xxh3_64;

use crate::NamespaceId;

/// Deterministic hash of a full query parameter set.
///
/// Constructed from a canonical byte representation of the query — the
/// caller is responsible for producing a stable canonicalisation (for
/// example `serde_json::to_vec` of a struct whose fields serialise in a
/// deterministic order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QueryHash(pub u64);

impl QueryHash {
    /// Hash an arbitrary byte slice into a `QueryHash`.
    pub fn of(bytes: &[u8]) -> Self {
        Self(xxh3_64(bytes))
    }
}

/// Full cache key for a namespaced query result.
///
/// The `generation` field is what makes invalidation O(1): bumping a
/// namespace's generation counter makes all previously populated
/// entries for that namespace unreachable by key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CacheKey {
    /// Namespace the query was scoped to.
    pub namespace: NamespaceId,
    /// Generation counter at the time the entry was populated.
    pub generation: u64,
    /// Hash of the query parameters.
    pub query: QueryHash,
}
