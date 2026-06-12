//! Embedded-mode integration test: `NamespaceManager` against a local
//! filesystem `StorageRoot` (no S3, no MinIO, no network).
//!
//! Proves the `Scheme::Local` path end-to-end: `lancedb::connect`
//! opens a local Lance table from the `file://` URI, an upsert writes
//! a directory tree under the base dir, a query reads it back, and the
//! delete path drops through `object_store::local::LocalFileSystem` to
//! remove the namespace's objects. Unlike the cloud manager tests this
//! one is deliberately **not** `#[ignore]`d — embedded mode is exactly
//! the zero-infrastructure case, so it runs in CI and locally as-is.

use std::collections::HashMap;

use firnflow_core::metrics::test_metrics;
use firnflow_core::{NamespaceId, NamespaceManager, StorageRoot, UpsertRow};
use tempfile::TempDir;

const DIM: usize = 8;

fn unit_vector(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    v[axis] = 1.0;
    v
}

fn local_manager(dir: &TempDir) -> NamespaceManager {
    NamespaceManager::new(
        StorageRoot::local(dir.path()).unwrap(),
        HashMap::new(),
        test_metrics(),
    )
}

#[tokio::test]
async fn local_fs_upsert_query_roundtrip() {
    let dir = TempDir::new().unwrap();
    let manager = local_manager(&dir);
    let ns = NamespaceId::new("embedded-roundtrip").unwrap();

    let rows: Vec<UpsertRow> = vec![
        (1u64, unit_vector(0)).into(),
        (2u64, unit_vector(1)).into(),
        (3u64, unit_vector(2)).into(),
    ];
    manager.upsert(&ns, rows).await.expect("local upsert");

    // The namespace directory must now exist on the local filesystem
    // under the base dir — proof that lancedb opened and wrote to the
    // `file://` root rather than silently failing or going elsewhere.
    assert!(
        dir.path().join("embedded-roundtrip").is_dir(),
        "expected a local Lance table directory for the namespace"
    );

    let results = manager
        .query(&ns, unit_vector(0), None, 3, None, None, true)
        .await
        .expect("local query");

    assert_eq!(results.results.len(), 3, "should return top-3 hits");
    let top = &results.results[0];
    assert_eq!(top.id, 1, "nearest neighbour of axis-0 must be id=1");
    assert!(
        top.score < 0.01,
        "self-distance should be ~0, got {}",
        top.score
    );
    let top_vector = top
        .vector
        .as_ref()
        .expect("default query must return the stored vector");
    assert_eq!(top_vector.len(), DIM, "returned vector width must match");
}

#[tokio::test]
async fn local_fs_fts_text_search() {
    // The path the Python binding's text/hybrid search relies on:
    // build a BM25 index over the text column, then run an FTS-only
    // query. Rows carry a vector (firn is a vector engine) plus text.
    let dir = TempDir::new().unwrap();
    let manager = local_manager(&dir);
    let ns = NamespaceId::new("embedded-fts").unwrap();

    let rows: Vec<UpsertRow> = vec![
        UpsertRow {
            id: 1,
            vector: unit_vector(0),
            vectors: None,
            text: Some("the quick brown fox".into()),
        },
        UpsertRow {
            id: 2,
            vector: unit_vector(1),
            vectors: None,
            text: Some("a lazy dog sleeps".into()),
        },
        UpsertRow {
            id: 3,
            vector: unit_vector(2),
            vectors: None,
            text: Some("the fox runs fast".into()),
        },
    ];
    manager.upsert(&ns, rows).await.expect("local upsert");
    manager
        .create_fts_index(&ns)
        .await
        .expect("create fts index");

    // FTS-only: empty vector, text set.
    let results = manager
        .query(&ns, Vec::new(), None, 10, None, Some("fox".into()), false)
        .await
        .expect("fts query");
    let ids: Vec<u64> = results.results.iter().map(|r| r.id).collect();
    assert!(
        ids.contains(&1) && ids.contains(&3),
        "'fox' should match rows 1 and 3, got {ids:?}"
    );
    assert!(
        !ids.contains(&2),
        "row 2 has no 'fox' and must not match, got {ids:?}"
    );
}

#[tokio::test]
async fn local_fs_delete_removes_namespace_objects() {
    let dir = TempDir::new().unwrap();
    let manager = local_manager(&dir);
    let ns = NamespaceId::new("embedded-delete").unwrap();

    let rows: Vec<UpsertRow> = vec![(1u64, unit_vector(0)).into()];
    manager.upsert(&ns, rows).await.expect("local upsert");
    assert!(dir.path().join("embedded-delete").is_dir());

    // Delete drops into the local object store, lists the namespace
    // prefix, and removes each object. At least the manifest + data
    // files should come back in the count.
    let deleted = manager.delete(&ns).await.expect("local delete");
    assert!(
        deleted > 0,
        "delete should remove at least one object, got {deleted}"
    );
}
