//! Multivector namespace integration test: `NamespaceManager`
//! round-trip with late-interaction (ColBERT-style) data shapes.
//!
//! Exercises every code path that branches on
//! [`firnflow_core::VectorKind::Multivector`]:
//!
//! - First upsert with a `vectors: Some(Vec<Vec<f32>>)` payload
//!   establishes the namespace as multivector with the right inner
//!   sub-vector dim.
//! - Subsequent upserts validate the payload shape against the
//!   namespace kind and reject mismatches.
//! - Queries with a `vectors:` payload return ranked hits.
//! - `QueryResult.vector` is `None` for multivector results (the bag
//!   is intentionally not echoed; see manager.rs `batches_to_results`).
//! - Wrong-shape payloads (single into multivector, multi into
//!   single) fail with clear errors at the manager boundary.
//! - Empty inner lists and mixed sub-vector dims fail validation.
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-core --test manager_multivector \
//!     -- --ignored --nocapture
//! ```

use std::collections::HashMap;

use firnflow_core::metrics::test_metrics;
use firnflow_core::{NamespaceId, NamespaceManager, StorageRoot, UpsertRow, VectorKind};

/// Inner sub-vector dim. Small enough to keep the test fast; large
/// enough that randomly aligned vectors don't accidentally produce
/// degenerate cosine distances, and large enough that an IVF_PQ
/// index built with `num_sub_vectors >= 2` has at least two
/// dimensions per PQ codebook (one codebook per axis pair is the
/// minimum PQ training stays meaningful under).
const SUB_DIM: usize = 8;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn unique_namespace(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{nanos}")
}

fn minio_options() -> HashMap<String, String> {
    HashMap::from([
        (
            "aws_access_key_id".into(),
            env_or("FIRNFLOW_S3_ACCESS_KEY", "minioadmin"),
        ),
        (
            "aws_secret_access_key".into(),
            env_or("FIRNFLOW_S3_SECRET_KEY", "minioadmin"),
        ),
        (
            "aws_endpoint".into(),
            env_or("FIRNFLOW_S3_ENDPOINT", "http://127.0.0.1:9000"),
        ),
        ("aws_region".into(), "us-east-1".into()),
        ("allow_http".into(), "true".into()),
        ("aws_virtual_hosted_style_request".into(), "false".into()),
    ])
}

/// Build a one-hot unit sub-vector along `axis`. Used as the
/// building block for synthetic multivector rows.
fn unit(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; SUB_DIM];
    v[axis] = 1.0;
    v
}

fn multi(subs: Vec<Vec<f32>>) -> UpsertRow {
    UpsertRow {
        id: 0,
        vector: Vec::new(),
        vectors: Some(subs),
        text: None,
        attributes: serde_json::Map::new(),
    }
}

fn with_id(mut row: UpsertRow, id: u64) -> UpsertRow {
    row.id = id;
    row
}

#[tokio::test]
#[ignore]
async fn upsert_establishes_multivector_kind_and_dim() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let manager = NamespaceManager::new(
        StorageRoot::s3_bucket(&bucket).unwrap(),
        minio_options(),
        test_metrics(),
    );
    let ns = NamespaceId::new(unique_namespace("mv-kind")).unwrap();

    // First upsert with a `vectors:` payload establishes the
    // namespace as multivector with inner dim = SUB_DIM.
    let rows = vec![
        with_id(multi(vec![unit(0), unit(1)]), 1),
        with_id(multi(vec![unit(2), unit(3), unit(0)]), 2),
    ];
    manager.upsert(&ns, rows).await.expect("first upsert");

    assert_eq!(
        manager.kind_for(&ns),
        Some(VectorKind::Multivector),
        "namespace kind must be Multivector after a multivector first upsert"
    );
    assert_eq!(
        manager.dim_for(&ns),
        Some(SUB_DIM),
        "inner sub-vector dim must match the payload"
    );
}

/// Seed a multivector namespace with enough rows for an IVF_PQ
/// index to actually train. Lance refuses to build PQ on fewer than
/// 256 sub-vectors. Row 1 is the "target" pattern (axes 0 + 1); row
/// 2 is the contrast pattern (axes 2 + 3); the remaining filler is
/// noise so the namespace has the training mass IVF_PQ requires
/// without polluting the top-of-rank for the target query.
async fn seed_multivector_namespace(manager: &NamespaceManager, ns: &NamespaceId) {
    let mut rows = vec![
        with_id(multi(vec![unit(0), unit(1)]), 1),
        with_id(multi(vec![unit(2), unit(3)]), 2),
    ];
    // 300 filler rows with 1 sub-vector each → 302 sub-vectors total,
    // safely above Lance's 256 PQ training floor. Each filler points
    // along a single axis cycling through every axis in `0..SUB_DIM`;
    // collectively they contribute mass to every cluster centroid.
    for i in 0..300u64 {
        let axis = (i as usize) % SUB_DIM;
        rows.push(with_id(multi(vec![unit(axis)]), 100 + i));
    }
    manager.upsert(ns, rows).await.expect("seed upsert");
}

#[tokio::test]
#[ignore]
async fn upsert_then_query_returns_multivector_hits() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let manager = NamespaceManager::new(
        StorageRoot::s3_bucket(&bucket).unwrap(),
        minio_options(),
        test_metrics(),
    );
    let ns = NamespaceId::new(unique_namespace("mv-roundtrip")).unwrap();

    seed_multivector_namespace(&manager, &ns).await;

    // Exact ranking is asserted against the brute-force (un-indexed)
    // multivector path. With no index built the query is a full MaxSim
    // scan, which is deterministic: row 1 shares both sub-vectors with
    // the query (MaxSim 2.0) and must outrank every filler row, which
    // shares at most one (MaxSim 1.0). This is the contract the engine
    // actually guarantees.
    let exact = manager
        .query(
            &ns,
            Vec::new(),
            Some(vec![unit(0), unit(1)]),
            3,
            None,
            None,
            None,
            true,
        )
        .await
        .expect("brute-force multivector query");
    assert_eq!(
        exact.results[0].id, 1,
        "row 1 must rank first under exact MaxSim against its own \
         pattern; top result was id={}",
        exact.results[0].id
    );
    assert!(
        exact.results[0].vector.is_none(),
        "multivector results must not echo the bag — got {:?}",
        exact.results[0].vector
    );

    // Build the IVF_PQ index and re-query as a smoke test of the indexed
    // late-interaction path. IVF_PQ is approximate: it quantises each
    // sub-vector into a lossy PQ code and makes no promise about the
    // order of rows whose true MaxSim scores are close (here a 2.0
    // target against 1.0 fillers). Asserting strict top-1 ranking
    // through it is stronger than the index guarantees and reorders run
    // to run as the k-means codebook varies between builds — see #53.
    // Assert only that the indexed path stays wired: it returns hits and
    // still omits the bag.
    manager
        .create_index(&ns, Some(4), Some(2), None)
        .await
        .expect("index build");

    let indexed = manager
        .query(
            &ns,
            Vec::new(),
            Some(vec![unit(0), unit(1)]),
            3,
            None,
            None,
            None,
            true,
        )
        .await
        .expect("indexed multivector query");
    assert!(
        !indexed.results.is_empty(),
        "indexed query must return hits"
    );
    assert!(
        indexed.results[0].vector.is_none(),
        "multivector results must not echo the bag — got {:?}",
        indexed.results[0].vector
    );
}

#[tokio::test]
#[ignore]
async fn single_payload_rejected_on_multivector_namespace() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let manager = NamespaceManager::new(
        StorageRoot::s3_bucket(&bucket).unwrap(),
        minio_options(),
        test_metrics(),
    );
    let ns = NamespaceId::new(unique_namespace("mv-shape-mismatch")).unwrap();

    // Establish multivector kind.
    let rows = vec![with_id(multi(vec![unit(0), unit(1)]), 1)];
    manager.upsert(&ns, rows).await.expect("first upsert");

    // A single-vector payload must be rejected with a clear message
    // naming the expected shape.
    let err = manager
        .upsert(&ns, vec![UpsertRow::from((2u64, unit(0)))])
        .await
        .expect_err("single payload on multivector namespace must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("multivector") && msg.contains("vectors:"),
        "error must name the expected `vectors:` shape; got: {msg}"
    );

    // Same on the query side.
    let err = manager
        .query(&ns, unit(0), None, 2, None, None, None, true)
        .await
        .expect_err("single query on multivector namespace must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("multivector"),
        "query error must call out the namespace kind; got: {msg}"
    );
}

#[tokio::test]
#[ignore]
async fn multi_payload_rejected_on_single_namespace() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let manager = NamespaceManager::new(
        StorageRoot::s3_bucket(&bucket).unwrap(),
        minio_options(),
        test_metrics(),
    );
    let ns = NamespaceId::new(unique_namespace("single-mismatch")).unwrap();

    // Establish single-vector kind.
    let rows: Vec<UpsertRow> = vec![(1u64, unit(0)).into()];
    manager.upsert(&ns, rows).await.expect("first upsert");

    // Multivector payload must be rejected.
    let err = manager
        .upsert(&ns, vec![with_id(multi(vec![unit(0), unit(1)]), 2)])
        .await
        .expect_err("multivector payload on single namespace must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("single") && msg.contains("vector:"),
        "error must name the expected `vector:` shape; got: {msg}"
    );

    // Same on the query side.
    let err = manager
        .query(
            &ns,
            Vec::new(),
            Some(vec![unit(0), unit(1)]),
            2,
            None,
            None,
            None,
            true,
        )
        .await
        .expect_err("multivector query on single namespace must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("single-vector"),
        "query error must call out the namespace kind; got: {msg}"
    );
}

#[tokio::test]
#[ignore]
async fn empty_inner_list_rejected() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let manager = NamespaceManager::new(
        StorageRoot::s3_bucket(&bucket).unwrap(),
        minio_options(),
        test_metrics(),
    );
    let ns = NamespaceId::new(unique_namespace("mv-empty")).unwrap();

    // A `vectors: [[]]` payload (one empty sub-vector) must be
    // rejected before reaching the Arrow builder. Pre-fix Lance
    // builds panicked on empty inner lists.
    let err = manager
        .upsert(&ns, vec![with_id(multi(vec![Vec::new()]), 1)])
        .await
        .expect_err("empty inner list must fail validation");
    let msg = format!("{err}");
    assert!(
        msg.contains("empty"),
        "error must mention the empty inner list; got: {msg}"
    );
}

#[tokio::test]
#[ignore]
async fn mixed_inner_dim_rejected() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let manager = NamespaceManager::new(
        StorageRoot::s3_bucket(&bucket).unwrap(),
        minio_options(),
        test_metrics(),
    );
    let ns = NamespaceId::new(unique_namespace("mv-mixed-dim")).unwrap();

    // Two sub-vectors of different lengths inside one row must fail
    // with a per-row diagnostic before the batch build.
    let mut bad = multi(vec![unit(0), vec![1.0, 0.0]]);
    bad.id = 1;
    let err = manager
        .upsert(&ns, vec![bad])
        .await
        .expect_err("mixed sub-vector dim must fail validation");
    let msg = format!("{err}");
    assert!(
        msg.contains("sub-vector") && msg.contains("length"),
        "error must mention the offending sub-vector length; got: {msg}"
    );
}

#[tokio::test]
#[ignore]
async fn create_index_forces_cosine_on_multivector() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let manager = NamespaceManager::new(
        StorageRoot::s3_bucket(&bucket).unwrap(),
        minio_options(),
        test_metrics(),
    );
    let ns = NamespaceId::new(unique_namespace("mv-index")).unwrap();

    seed_multivector_namespace(&manager, &ns).await;

    // create_index forces DistanceType::Cosine for multivector
    // namespaces regardless of what the caller passes. The
    // observable signal here is that the call succeeds — Lance
    // would reject a non-cosine metric at build time with
    // "multivector type supports only cosine distance".
    manager
        .create_index(&ns, Some(4), Some(1), None)
        .await
        .expect("multivector index build under forced cosine");

    // A multivector query after index build must still succeed.
    let results = manager
        .query(
            &ns,
            Vec::new(),
            Some(vec![unit(0), unit(1)]),
            5,
            None,
            None,
            None,
            true,
        )
        .await
        .expect("post-index multivector query");
    assert!(
        !results.results.is_empty(),
        "indexed multivector query must return at least one hit"
    );
}
