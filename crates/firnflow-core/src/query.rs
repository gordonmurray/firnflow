//! Query and index request types.
//!
//! Request payloads are kept in their own module so that `result.rs`
//! stays focused on the response side. Both modules share the same
//! serde derives and are what the axum handlers parse straight from
//! request bodies.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// Default number of IVF partitions to probe per query when an
/// index exists. Matches lancedb's own default (20).
pub const DEFAULT_NPROBES: usize = 20;

/// Parameters of a search query.
///
/// Supports three query modes depending on which fields are set:
/// - **Vector-only**: `vector` or `vectors` set, `text` absent →
///   nearest-neighbour search.
/// - **FTS-only**: `text` set, both vector fields absent → BM25
///   full-text search.
/// - **Hybrid**: a vector field and `text` both set → combined
///   vector + FTS via Reciprocal Rank Fusion.
///
/// The vector payload field is determined by the namespace's kind:
/// - **Single-vector namespaces** accept `vector: [f32, ...]`.
/// - **Multivector namespaces** accept
///   `vectors: [[f32, ...], [f32, ...]]` — a bag of small vectors,
///   each of the namespace's inner dimension.
///
/// At most one of `vector` / `vectors` may be set; setting both
/// returns 400. The payload shape must match the namespace's kind
/// (a single-vector namespace receiving a `vectors` payload, or
/// vice-versa, returns 400 with the expected shape spelled out).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryRequest {
    /// The single-vector query payload. Length must match the
    /// namespace's dimension. Empty / absent means "no single vector".
    /// Mutually exclusive with [`vectors`](Self::vectors).
    #[serde(default)]
    pub vector: Vec<f32>,
    /// The multivector query payload. Each inner vector must have
    /// the namespace's inner sub-vector dimension; the outer list
    /// length is the per-query sub-vector count. `None` means "no
    /// multivector". Mutually exclusive with [`vector`](Self::vector).
    #[serde(default)]
    pub vectors: Option<Vec<Vec<f32>>>,
    /// Maximum number of results to return.
    pub k: usize,
    /// Number of IVF partitions to probe. Only meaningful when an
    /// index exists; ignored for linear scans. Defaults to 20 if
    /// omitted.
    #[serde(default)]
    pub nprobes: Option<usize>,
    /// Full-text search query string. When set alongside a vector
    /// field, triggers hybrid search (vector + FTS combined via RRF).
    /// When set without any vector field, triggers FTS-only search.
    #[serde(default)]
    pub text: Option<String>,
    /// Optional DataFusion SQL predicate, using the same dialect as
    /// `/list` filters. LanceDB applies this as a prefilter before
    /// nearest-neighbour search, so vector queries return up to `k`
    /// neighbours satisfying the predicate rather than filtering an
    /// already-selected top-k.
    #[serde(default)]
    pub filter: Option<String>,
    /// Whether result rows carry the stored vector. Defaults to
    /// `true`, preserving the existing response shape. `false` asks
    /// the backend not to materialise or return the stored vector
    /// column — at realistic dimensions the vector dominates the
    /// response payload (6 KiB of raw `f32` per hit at dim=1536),
    /// so callers that only need `id`/`score`/`text` save the
    /// transfer, the cache bytes, and the parse cost.
    ///
    /// Participates in the exact-cache key: a full and a vector-light
    /// result for the same search are different payloads and must not
    /// collide.
    #[serde(default = "default_include_vector")]
    pub include_vector: bool,
    /// Opt-in semantic-cache controls. Absent or `enabled: false`
    /// leaves the exact result cache as the only short-circuit;
    /// `enabled: true` permits reusing the result of a previous
    /// very-similar query when the exact cache misses. The default
    /// shape is "off", so existing callers see no behaviour change.
    ///
    /// Excluded from the exact-cache key so toggling the option
    /// does not split otherwise-identical entries.
    #[serde(default)]
    pub semantic_cache: Option<SemanticCacheRequest>,
}

/// Request payload for `POST /ns/{namespace}/facet`.
///
/// Facets are computed over the whole set matching `filter`, not over
/// a vector query's returned top-k. The filter dialect is the same
/// DataFusion SQL predicate accepted by `/query` and `/list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FacetRequest {
    /// Optional DataFusion SQL predicate.
    #[serde(default)]
    pub filter: Option<String>,
    /// Scalar fields to aggregate.
    pub fields: Vec<String>,
    /// Maximum buckets to return per field. Defaults at the service
    /// boundary when omitted.
    #[serde(default)]
    pub top: Option<usize>,
}

/// Default bucket cap for facets.
pub const DEFAULT_FACET_TOP: usize = 100;
/// Maximum bucket cap accepted by the facet endpoint.
pub const MAX_FACET_TOP: usize = 1000;

/// Pure shape validation for a facet request. Column existence and
/// facetable-ness are checked by the manager against the live schema.
pub fn validate_facet_request(req: &FacetRequest) -> Result<usize, crate::FirnflowError> {
    if req.fields.is_empty() {
        return Err(crate::FirnflowError::InvalidRequest(
            "facet fields must not be empty".into(),
        ));
    }
    let mut seen = HashSet::with_capacity(req.fields.len());
    for field in &req.fields {
        if !seen.insert(field) {
            return Err(crate::FirnflowError::InvalidRequest(format!(
                "duplicate facet field '{field}'"
            )));
        }
    }
    let top = req.top.unwrap_or(DEFAULT_FACET_TOP);
    if top == 0 || top > MAX_FACET_TOP {
        return Err(crate::FirnflowError::InvalidRequest(format!(
            "facet top must be in 1..={MAX_FACET_TOP}, got {top}"
        )));
    }
    Ok(top)
}

/// Per-request controls for opt-in semantic caching.
///
/// Semantic caching is approximate: a hit means the cached result
/// belonged to a previous query whose vector was extremely close
/// (cosine similarity `>= min_similarity`) and whose surrounding
/// request shape was identical (`k`, `nprobes`, single-vector,
/// no text/filters). Eligible namespaces are single-vector only in
/// v1. Multivector, FTS, and hybrid queries with semantic caching
/// requested are rejected with HTTP 400.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticCacheRequest {
    /// When `true`, the read path may reuse a previous near-duplicate
    /// query's cached result. When `false` (or the field is absent),
    /// only exact-cache hits short-circuit.
    pub enabled: bool,
    /// Cosine-similarity floor for a semantic hit. Must be in
    /// `(0.0, 1.0]`. Omitting picks the server default
    /// ([`DEFAULT_SEMANTIC_MIN_SIMILARITY`]) — deliberately strict
    /// because a semantic hit returns an approximate top-k, not
    /// the exact one.
    #[serde(default)]
    pub min_similarity: Option<f32>,
}

/// Serde default for [`QueryRequest::include_vector`] — vectors are
/// returned unless the caller opts out.
fn default_include_vector() -> bool {
    true
}

/// Conservative default cosine threshold for semantic-cache hits.
///
/// Picked to be strict enough that two queries reaching this
/// threshold are very likely to ask for the same top-k against any
/// reasonable embedding model. Operators may relax it per request
/// via [`SemanticCacheRequest::min_similarity`].
pub const DEFAULT_SEMANTIC_MIN_SIMILARITY: f32 = 0.995;

/// Validate the opt-in semantic-cache controls carried by a
/// [`QueryRequest`].
///
/// Pure: no I/O. Called from `NamespaceService::query` before any
/// cache or backend work so a bad payload returns 400 immediately.
///
/// Checks:
/// - `min_similarity ∈ (0.0, 1.0]` when supplied.
/// - When `enabled: true`, the request must be single-vector
///   (`vector` non-empty, `vectors` absent, `text` and `filter`
///   absent). V1 does not support semantic caching for FTS, hybrid,
///   filtered, or multivector queries — those reject with a clear 400.
pub fn validate_semantic_cache_request(req: &QueryRequest) -> Result<(), crate::FirnflowError> {
    let Some(sem) = req.semantic_cache.as_ref() else {
        return Ok(());
    };
    if let Some(threshold) = sem.min_similarity {
        if !threshold.is_finite() || threshold <= 0.0 || threshold > 1.0 {
            return Err(crate::FirnflowError::InvalidRequest(format!(
                "semantic_cache.min_similarity must be in (0.0, 1.0], got {threshold}"
            )));
        }
    }
    if !sem.enabled {
        return Ok(());
    }
    if req.vector.is_empty() {
        return Err(crate::FirnflowError::InvalidRequest(
            "semantic_cache requires a single-vector `vector` field; \
             multivector, FTS, and hybrid queries are not eligible in v1"
                .into(),
        ));
    }
    if req.vectors.as_ref().is_some_and(|v| !v.is_empty()) {
        return Err(crate::FirnflowError::InvalidRequest(
            "semantic_cache is not supported for multivector queries in v1".into(),
        ));
    }
    if req.text.is_some() {
        return Err(crate::FirnflowError::InvalidRequest(
            "semantic_cache is not supported for FTS or hybrid queries in v1".into(),
        ));
    }
    if req.filter.is_some() {
        return Err(crate::FirnflowError::InvalidRequest(
            "semantic_cache is not supported for filtered queries in v1".into(),
        ));
    }
    Ok(())
}

/// Effective cosine threshold for a query: the per-request override
/// if supplied, else [`DEFAULT_SEMANTIC_MIN_SIMILARITY`].
pub fn effective_semantic_threshold(req: &SemanticCacheRequest) -> f32 {
    req.min_similarity
        .unwrap_or(DEFAULT_SEMANTIC_MIN_SIMILARITY)
}

/// Parameters for an explicit index build request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexRequest {
    /// Index type. Currently only `"ivf_pq"` is supported.
    #[serde(default = "default_index_kind")]
    pub kind: String,
    /// Number of IVF partitions. Defaults to `sqrt(row_count)` if
    /// omitted.
    pub num_partitions: Option<u32>,
    /// Number of PQ sub-vectors. Defaults to `dim / 16` if omitted.
    pub num_sub_vectors: Option<u32>,
    /// PQ codebook bit width. `Some(4)` halves the per-vector index
    /// storage cost relative to the 8-bit default at the cost of
    /// some recall. Accepted values are 4 or 8; omitting keeps
    /// Lance's 8-bit default.
    ///
    /// `Some(4)` additionally requires `num_sub_vectors` to be even
    /// (Lance 6 rejects 4-bit PQ over an odd sub-vector count).
    #[serde(default)]
    pub num_bits: Option<u32>,
}

fn default_index_kind() -> String {
    "ivf_pq".into()
}

/// Validate the IVF_PQ tuning options carried by an [`IndexRequest`].
///
/// Pure: no I/O. Two intended call sites: the API handler calls
/// this synchronously before spawning the index build so a bad
/// payload returns 400 immediately, and
/// [`crate::NamespaceManager::create_index`] calls it before the
/// `IvfPqIndexBuilder` step so direct callers (benches, integration
/// tests) bypassing the API are still protected.
///
/// Checks:
/// - `num_bits` ∈ {`None`, `Some(4)`, `Some(8)`}. Lance only
///   supports 4 and 8.
/// - `num_bits == Some(4)` combined with an explicit odd
///   `num_sub_vectors` is rejected. Lance 6 requires the sub-vector
///   count to be even for 4-bit PQ.
pub fn validate_ivf_pq_options(
    num_bits: Option<u32>,
    num_sub_vectors: Option<u32>,
) -> Result<(), crate::FirnflowError> {
    if let Some(bits) = num_bits {
        if bits != 4 && bits != 8 {
            return Err(crate::FirnflowError::InvalidRequest(format!(
                "num_bits={bits} is not supported; accepted values are 4 or 8"
            )));
        }
        if bits == 4 {
            if let Some(n) = num_sub_vectors {
                if n % 2 != 0 {
                    return Err(crate::FirnflowError::InvalidRequest(format!(
                        "num_bits=4 requires num_sub_vectors to be even (got {n})"
                    )));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FirnflowError;

    #[test]
    fn validate_accepts_none_and_supported_widths() {
        assert!(validate_ivf_pq_options(None, None).is_ok());
        assert!(validate_ivf_pq_options(None, Some(63)).is_ok());
        assert!(validate_ivf_pq_options(Some(4), None).is_ok());
        assert!(validate_ivf_pq_options(Some(4), Some(64)).is_ok());
        assert!(validate_ivf_pq_options(Some(8), Some(63)).is_ok());
    }

    #[test]
    fn validate_rejects_unsupported_bit_width() {
        let err = validate_ivf_pq_options(Some(7), None).unwrap_err();
        match err {
            FirnflowError::InvalidRequest(msg) => {
                assert!(msg.contains("num_bits=7"), "{msg}");
                assert!(msg.contains("4 or 8"), "{msg}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_4bit_with_odd_sub_vectors() {
        let err = validate_ivf_pq_options(Some(4), Some(63)).unwrap_err();
        match err {
            FirnflowError::InvalidRequest(msg) => {
                assert!(msg.contains("num_bits=4"), "{msg}");
                assert!(msg.contains("even"), "{msg}");
                assert!(msg.contains("63"), "{msg}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn validate_allows_4bit_with_unspecified_sub_vectors() {
        // num_sub_vectors omitted means "use Lance's default" — the
        // default is dim/16 which is even for any vector dim that is
        // a multiple of 32 (the realistic case). Don't reject here.
        assert!(validate_ivf_pq_options(Some(4), None).is_ok());
    }

    fn req_vector_only() -> QueryRequest {
        QueryRequest {
            vector: vec![0.1, 0.2, 0.3],
            vectors: None,
            k: 10,
            nprobes: None,
            text: None,
            filter: None,
            include_vector: true,
            semantic_cache: None,
        }
    }

    #[test]
    fn semantic_cache_absent_or_disabled_is_ok() {
        let mut req = req_vector_only();
        assert!(validate_semantic_cache_request(&req).is_ok());

        req.semantic_cache = Some(SemanticCacheRequest {
            enabled: false,
            min_similarity: None,
        });
        assert!(validate_semantic_cache_request(&req).is_ok());
    }

    #[test]
    fn semantic_cache_threshold_range_enforced() {
        let mut req = req_vector_only();
        req.semantic_cache = Some(SemanticCacheRequest {
            enabled: true,
            min_similarity: Some(0.0),
        });
        let err = validate_semantic_cache_request(&req).unwrap_err();
        match err {
            FirnflowError::InvalidRequest(msg) => assert!(msg.contains("min_similarity"), "{msg}"),
            other => panic!("expected InvalidRequest, got {other:?}"),
        }

        let mut req = req_vector_only();
        req.semantic_cache = Some(SemanticCacheRequest {
            enabled: true,
            min_similarity: Some(1.5),
        });
        assert!(matches!(
            validate_semantic_cache_request(&req).unwrap_err(),
            FirnflowError::InvalidRequest(_)
        ));

        let mut req = req_vector_only();
        req.semantic_cache = Some(SemanticCacheRequest {
            enabled: true,
            min_similarity: Some(f32::NAN),
        });
        assert!(matches!(
            validate_semantic_cache_request(&req).unwrap_err(),
            FirnflowError::InvalidRequest(_)
        ));
    }

    #[test]
    fn semantic_cache_rejects_fts_hybrid_and_multivector() {
        let mut req = req_vector_only();
        req.text = Some("hello".into());
        req.semantic_cache = Some(SemanticCacheRequest {
            enabled: true,
            min_similarity: None,
        });
        let err = validate_semantic_cache_request(&req).unwrap_err();
        let msg = match err {
            FirnflowError::InvalidRequest(m) => m,
            other => panic!("expected InvalidRequest, got {other:?}"),
        };
        assert!(msg.contains("FTS") || msg.contains("hybrid"), "{msg}");

        let mut req = req_vector_only();
        req.vector.clear();
        req.vectors = Some(vec![vec![0.1, 0.2]]);
        req.semantic_cache = Some(SemanticCacheRequest {
            enabled: true,
            min_similarity: None,
        });
        let err = validate_semantic_cache_request(&req).unwrap_err();
        match err {
            FirnflowError::InvalidRequest(msg) => {
                assert!(
                    msg.contains("multivector") || msg.contains("single-vector"),
                    "{msg}"
                );
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn semantic_cache_requires_a_vector_when_enabled() {
        let mut req = req_vector_only();
        req.vector.clear();
        req.semantic_cache = Some(SemanticCacheRequest {
            enabled: true,
            min_similarity: None,
        });
        let err = validate_semantic_cache_request(&req).unwrap_err();
        match err {
            FirnflowError::InvalidRequest(msg) => assert!(msg.contains("single-vector"), "{msg}"),
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn semantic_cache_rejects_filter() {
        let mut req = req_vector_only();
        req.filter = Some("id < 5".into());
        req.semantic_cache = Some(SemanticCacheRequest {
            enabled: true,
            min_similarity: None,
        });

        let err = validate_semantic_cache_request(&req).unwrap_err();
        match err {
            FirnflowError::InvalidRequest(msg) => {
                assert!(msg.contains("filter"), "{msg}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn facet_request_validation() {
        let mut req = FacetRequest {
            filter: None,
            fields: vec!["section".into()],
            top: None,
        };
        assert_eq!(validate_facet_request(&req).unwrap(), DEFAULT_FACET_TOP);

        req.fields.clear();
        assert!(matches!(
            validate_facet_request(&req),
            Err(FirnflowError::InvalidRequest(_))
        ));

        req.fields.push("section".into());
        req.top = Some(0);
        assert!(matches!(
            validate_facet_request(&req),
            Err(FirnflowError::InvalidRequest(_))
        ));

        req.top = Some(10);
        req.fields.push("section".into());
        assert!(matches!(
            validate_facet_request(&req),
            Err(FirnflowError::InvalidRequest(_))
        ));
    }
}
