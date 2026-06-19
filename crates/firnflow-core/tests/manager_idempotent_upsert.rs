//! Idempotent-upsert integration tests for `NamespaceManager`.
//!
//! `/upsert` is latest-write-wins keyed by `id` (LanceDB merge-insert),
//! not append-only. These tests prove:
//!
//! 1. Re-upserting an existing `id` replaces the row in place; the
//!    namespace row count does not grow and the stored data is the
//!    newer value.
//! 2. `_ingested_at` advances on a re-upsert (latest-write semantics).
//! 3. Duplicate ids within a single request are rejected with 400
//!    before any write (Lance leaves multi-match merge undefined).
//! 4. Merge-insert is keyed correctly on multivector namespaces too.
//! 5. Concurrent writers racing the same `id` converge on a single
//!    row, with no duplicates and no lost neighbours.
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-core --test manager_idempotent_upsert \
//!     -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use firnflow_core::metrics::test_metrics;
use firnflow_core::{
    FirnflowError, ListOrder, NamespaceId, NamespaceManager, StorageRoot, UpsertRow,
};
use tokio::sync::Barrier;

const DIM: usize = 8;

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

fn manager() -> NamespaceManager {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    NamespaceManager::new(
        StorageRoot::s3_bucket(&bucket).unwrap(),
        minio_options(),
        test_metrics(),
    )
}

fn unit_vector(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    v[axis] = 1.0;
    v
}

fn row(id: u64, vector: Vec<f32>, text: &str) -> UpsertRow {
    UpsertRow {
        id,
        vector,
        vectors: None,
        text: Some(text.to_string()),
    }
}

#[tokio::test]
#[ignore]
async fn reupsert_replaces_single_vector_row() {
    let manager = manager();
    let ns = NamespaceId::new(unique_namespace("idem-replace")).unwrap();

    // Seed two rows. id=2 is a bystander that must survive untouched
    // when id=1 is later re-upserted.
    manager
        .upsert(
            &ns,
            vec![
                row(1, unit_vector(0), "v1"),
                row(2, unit_vector(1), "bystander"),
            ],
        )
        .await
        .expect("initial upsert");

    let info = manager.info(&ns).await.expect("info").expect("namespace");
    assert_eq!(info.row_count, 2, "two distinct ids seeded");

    // Re-upsert id=1 with a different vector and text. This is the
    // case append-only got wrong: a second row for id=1 would appear.
    manager
        .upsert(&ns, vec![row(1, unit_vector(2), "v2")])
        .await
        .expect("re-upsert id=1");

    let info = manager.info(&ns).await.expect("info").expect("namespace");
    assert_eq!(
        info.row_count, 2,
        "re-upserting an existing id must replace, not append (still 2 rows)"
    );

    // The stored row for id=1 must now be the newer value. Query with
    // the new vector; id=1 is the top hit, carries the new text, and
    // appears exactly once.
    let results = manager
        .query(&ns, unit_vector(2), None, 10, None, None, true)
        .await
        .expect("query")
        .results;
    let id1_hits: Vec<_> = results.iter().filter(|r| r.id == 1).collect();
    assert_eq!(
        id1_hits.len(),
        1,
        "exactly one live row for id=1 after re-upsert"
    );
    assert_eq!(id1_hits[0].id, results[0].id, "id=1 is the nearest hit");
    assert_eq!(
        id1_hits[0].text.as_deref(),
        Some("v2"),
        "id=1 carries the re-upserted text"
    );
    assert!(
        results.iter().any(|r| r.id == 2),
        "the bystander row id=2 survives the re-upsert of id=1"
    );
}

#[tokio::test]
#[ignore]
async fn reupsert_advances_ingested_at() {
    let manager = manager();
    let ns = NamespaceId::new(unique_namespace("idem-ts")).unwrap();

    manager
        .upsert(&ns, vec![row(1, unit_vector(0), "first")])
        .await
        .expect("initial upsert");
    let first = manager
        .list(&ns, 50, ListOrder::Desc, None)
        .await
        .expect("list")
        .rows
        .into_iter()
        .find(|r| r.id == 1)
        .expect("row 1 present")
        .ingested_at_micros;

    // Sleep so the second write lands on a strictly later microsecond.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    manager
        .upsert(&ns, vec![row(1, unit_vector(0), "second")])
        .await
        .expect("re-upsert");
    let second = manager
        .list(&ns, 50, ListOrder::Desc, None)
        .await
        .expect("list")
        .rows
        .into_iter()
        .find(|r| r.id == 1)
        .expect("row 1 present")
        .ingested_at_micros;

    assert!(
        second > first,
        "re-upsert must advance _ingested_at (latest-write-wins): {first} -> {second}"
    );
}

#[tokio::test]
#[ignore]
async fn duplicate_ids_in_request_rejected() {
    let manager = manager();
    let ns = NamespaceId::new(unique_namespace("idem-dup")).unwrap();

    // Two rows with the same id in one request is ambiguous for
    // merge-insert, so it is rejected before any write. This returns
    // before touching object storage.
    let err = manager
        .upsert(
            &ns,
            vec![row(1, unit_vector(0), "a"), row(1, unit_vector(1), "b")],
        )
        .await
        .expect_err("duplicate ids in one request must be rejected");
    match err {
        FirnflowError::InvalidRequest(msg) => {
            assert!(msg.contains("duplicate id 1"), "unexpected message: {msg}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }

    // Nothing was written: the namespace does not exist yet.
    assert!(
        manager.info(&ns).await.expect("info").is_none(),
        "a rejected request must not create the namespace"
    );
}

#[tokio::test]
#[ignore]
async fn multivector_reupsert_replaces_not_appends() {
    let manager = manager();
    let ns = NamespaceId::new(unique_namespace("idem-mv")).unwrap();

    let bag_a = vec![unit_vector(0), unit_vector(1)];
    let bag_b = vec![unit_vector(2), unit_vector(3)];

    let mv_row = |id: u64, bag: Vec<Vec<f32>>| UpsertRow {
        id,
        vector: Vec::new(),
        vectors: Some(bag),
        text: None,
    };

    manager
        .upsert(&ns, vec![mv_row(1, bag_a)])
        .await
        .expect("initial multivector upsert");
    assert_eq!(
        manager
            .info(&ns)
            .await
            .expect("info")
            .expect("ns")
            .row_count,
        1
    );

    // Re-upsert the same id with a different bag. Merge-insert keys on
    // id regardless of the (List<FixedSizeList>) vector column shape.
    manager
        .upsert(&ns, vec![mv_row(1, bag_b)])
        .await
        .expect("multivector re-upsert");
    assert_eq!(
        manager
            .info(&ns)
            .await
            .expect("info")
            .expect("ns")
            .row_count,
        1,
        "re-upserting a multivector id replaces, not appends"
    );

    let results = manager
        .query(
            &ns,
            Vec::new(),
            Some(vec![unit_vector(2)]),
            10,
            None,
            None,
            true,
        )
        .await
        .expect("multivector query")
        .results;
    assert_eq!(
        results.iter().filter(|r| r.id == 1).count(),
        1,
        "exactly one live row for the re-upserted multivector id"
    );
}

#[tokio::test]
#[ignore]
async fn concurrent_merge_insert_same_id_keeps_single_row() {
    let manager = Arc::new(manager());
    let ns = NamespaceId::new(unique_namespace("idem-concurrent")).unwrap();

    // Seed the namespace so the table exists and the kind/dim are
    // fixed before the race. id=2 is a bystander; the writers all
    // contend on id=1. Seeding first means we test concurrent
    // merge-insert on the same key, not concurrent table creation.
    manager
        .upsert(
            &ns,
            vec![
                row(1, unit_vector(0), "seed"),
                row(2, unit_vector(1), "bystander"),
            ],
        )
        .await
        .expect("seed upsert");

    // Release all writers from the same starting line so the PUTs are
    // genuinely in flight together, not a sequential drip.
    const WRITERS: usize = 8;
    let barrier = Arc::new(Barrier::new(WRITERS));
    let mut handles = Vec::with_capacity(WRITERS);
    for w in 0..WRITERS {
        let manager = Arc::clone(&manager);
        let ns = ns.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            // Each writer upserts id=1 with a distinct vector. The
            // winning value is racy; what must hold is that exactly
            // one row for id=1 survives.
            manager
                .upsert(&ns, vec![row(1, unit_vector(w % DIM), "race")])
                .await
        }));
    }

    for h in handles {
        h.await
            .expect("task join")
            .expect("concurrent merge-insert must not error");
    }

    let info = manager.info(&ns).await.expect("info").expect("namespace");
    assert_eq!(
        info.row_count, 2,
        "after {WRITERS} writers race id=1, exactly one id=1 row and the \
         untouched id=2 row remain (no duplicates, no lost neighbour)"
    );
}

/// A namespace's first write auto-builds a BTree on `id` so later
/// merge-insert batches find their matches through the index instead
/// of scanning every fragment (issue #77). The maintenance path —
/// building the same index explicitly through `create_scalar_index`
/// — must also succeed and be idempotent for namespaces created
/// before auto-indexing existed.
#[tokio::test]
#[ignore]
async fn first_upsert_auto_builds_id_index() {
    let manager = manager();
    let ns = NamespaceId::new(unique_namespace("idem-idindex")).unwrap();

    manager
        .upsert(&ns, vec![row(1, unit_vector(0), "v1")])
        .await
        .expect("first upsert");

    let info = manager.info(&ns).await.expect("info").expect("namespace");
    assert!(
        info.has_scalar_index,
        "the first write must auto-build a scalar (BTree) index on id"
    );
    assert_eq!(info.row_count, 1, "the auto-index must not drop the row");

    // A second write into the now-indexed namespace still merges
    // correctly (the index is on the write-path lookup, not a barrier).
    manager
        .upsert(&ns, vec![row(2, unit_vector(1), "v2")])
        .await
        .expect("second upsert");
    let info = manager.info(&ns).await.expect("info").expect("namespace");
    assert_eq!(info.row_count, 2, "second distinct id appends a row");

    // Maintenance path: building the id index explicitly is idempotent
    // (lancedb's IndexBuilder defaults to replace=true), so an operator
    // can run it on a pre-existing namespace without it failing.
    manager
        .create_scalar_index(&ns, "id")
        .await
        .expect("explicit id index rebuild");
    let info = manager.info(&ns).await.expect("info").expect("namespace");
    assert!(
        info.has_scalar_index,
        "id index still present after rebuild"
    );
}
