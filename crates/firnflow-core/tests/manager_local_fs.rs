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
use firnflow_core::{FirnflowError, NamespaceId, NamespaceManager, StorageRoot, UpsertRow};
use serde_json::json;
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
        .query(&ns, unit_vector(0), None, 3, None, None, None, true)
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
            attributes: serde_json::Map::new(),
        },
        UpsertRow {
            id: 2,
            vector: unit_vector(1),
            vectors: None,
            text: Some("a lazy dog sleeps".into()),
            attributes: serde_json::Map::new(),
        },
        UpsertRow {
            id: 3,
            vector: unit_vector(2),
            vectors: None,
            text: Some("the fox runs fast".into()),
            attributes: serde_json::Map::new(),
        },
    ];
    manager.upsert(&ns, rows).await.expect("local upsert");
    manager
        .create_fts_index(&ns)
        .await
        .expect("create fts index");

    // FTS-only: empty vector, text set.
    let results = manager
        .query(
            &ns,
            Vec::new(),
            None,
            10,
            None,
            Some("fox".into()),
            None,
            false,
        )
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
async fn local_fs_query_filter_narrows_vector_results() {
    let dir = TempDir::new().unwrap();
    let manager = local_manager(&dir);
    let ns = NamespaceId::new("embedded-filter-vector").unwrap();

    let rows: Vec<UpsertRow> = vec![
        (1u64, unit_vector(0)).into(),
        (2u64, unit_vector(1)).into(),
        (3u64, unit_vector(2)).into(),
    ];
    manager.upsert(&ns, rows).await.expect("local upsert");

    let results = manager
        .query(
            &ns,
            unit_vector(0),
            None,
            3,
            None,
            None,
            Some("id > 1".into()),
            false,
        )
        .await
        .expect("filtered vector query");
    let mut ids: Vec<u64> = results.results.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![2, 3], "filter should exclude id=1");
}

#[tokio::test]
async fn local_fs_query_filter_accepts_ingested_at_ranges() {
    let dir = TempDir::new().unwrap();
    let manager = local_manager(&dir);
    let ns = NamespaceId::new("embedded-filter-ingested").unwrap();

    let rows: Vec<UpsertRow> = vec![(1u64, unit_vector(0)).into(), (2u64, unit_vector(1)).into()];
    manager.upsert(&ns, rows).await.expect("local upsert");

    let all = manager
        .query(&ns, unit_vector(0), None, 2, None, None, None, false)
        .await
        .expect("unfiltered query");
    let cutoff = all.results[0]
        .ingested_at_micros
        .expect("ingested_at on query hit");

    let results = manager
        .query(
            &ns,
            unit_vector(0),
            None,
            2,
            None,
            None,
            Some(format!("_ingested_at >= to_timestamp_micros({cutoff})")),
            false,
        )
        .await
        .expect("filtered ingested_at query");
    assert_eq!(results.results.len(), 2);
}

#[tokio::test]
async fn local_fs_query_filter_narrows_fts_and_hybrid_results() {
    let dir = TempDir::new().unwrap();
    let manager = local_manager(&dir);
    let ns = NamespaceId::new("embedded-filter-fts-hybrid").unwrap();

    let rows: Vec<UpsertRow> = vec![
        UpsertRow {
            id: 1,
            vector: unit_vector(0),
            vectors: None,
            text: Some("fox warning".into()),
            attributes: serde_json::Map::new(),
        },
        UpsertRow {
            id: 2,
            vector: unit_vector(1),
            vectors: None,
            text: Some("fox dosing".into()),
            attributes: serde_json::Map::new(),
        },
        UpsertRow {
            id: 3,
            vector: unit_vector(2),
            vectors: None,
            text: Some("dog warning".into()),
            attributes: serde_json::Map::new(),
        },
    ];
    manager.upsert(&ns, rows).await.expect("local upsert");
    manager
        .create_fts_index(&ns)
        .await
        .expect("create fts index");

    let fts = manager
        .query(
            &ns,
            Vec::new(),
            None,
            10,
            None,
            Some("fox".into()),
            Some("id = 2".into()),
            false,
        )
        .await
        .expect("filtered fts query");
    let fts_ids: Vec<u64> = fts.results.iter().map(|r| r.id).collect();
    assert_eq!(fts_ids, vec![2]);

    let hybrid = manager
        .query(
            &ns,
            unit_vector(0),
            None,
            10,
            None,
            Some("fox".into()),
            Some("id = 2".into()),
            false,
        )
        .await
        .expect("filtered hybrid query");
    let hybrid_ids: Vec<u64> = hybrid.results.iter().map(|r| r.id).collect();
    assert_eq!(hybrid_ids, vec![2]);
}

#[tokio::test]
async fn local_fs_facet_counts_attributes_with_filter_null_and_truncation() {
    let dir = TempDir::new().unwrap();
    let manager = local_manager(&dir);
    let ns = NamespaceId::new("embedded-facet").unwrap();

    let attr = |section: serde_json::Value, route: Option<&str>| {
        let mut attributes = serde_json::Map::new();
        attributes.insert("section".into(), section);
        if let Some(route) = route {
            attributes.insert("route".into(), json!(route));
        }
        attributes
    };
    manager
        .upsert(
            &ns,
            vec![
                UpsertRow {
                    id: 1,
                    vector: unit_vector(0),
                    vectors: None,
                    text: Some("warning oral".into()),
                    attributes: attr(json!("warnings"), Some("oral")),
                },
                UpsertRow {
                    id: 2,
                    vector: unit_vector(1),
                    vectors: None,
                    text: Some("dosage oral".into()),
                    attributes: attr(json!("dosage"), Some("oral")),
                },
                UpsertRow {
                    id: 3,
                    vector: unit_vector(2),
                    vectors: None,
                    text: Some("warning missing".into()),
                    attributes: attr(json!("warnings"), None),
                },
            ],
        )
        .await
        .expect("upsert");

    let result = manager
        .facet(
            &ns,
            Some("id >= 1".into()),
            &["section".into(), "route".into()],
            1,
        )
        .await
        .expect("facet");

    let section = result.facets.iter().find(|f| f.field == "section").unwrap();
    assert!(section.truncated);
    assert_eq!(section.buckets[0].value, json!("warnings"));
    assert_eq!(section.buckets[0].count, 2);

    let route = result.facets.iter().find(|f| f.field == "route").unwrap();
    assert!(route.truncated);
    assert_eq!(route.buckets[0].value, json!("oral"));
    assert_eq!(route.buckets[0].count, 2);

    let narrowed = manager
        .facet(
            &ns,
            Some("section = 'dosage'".into()),
            &["section".into()],
            10,
        )
        .await
        .expect("filtered facet");
    assert_eq!(narrowed.facets[0].buckets.len(), 1);
    assert_eq!(narrowed.facets[0].buckets[0].value, json!("dosage"));
    assert_eq!(narrowed.facets[0].buckets[0].count, 1);

    manager
        .create_scalar_index(&ns, "section")
        .await
        .expect("attribute scalar index");
}

#[tokio::test]
async fn local_fs_facet_rejects_bad_filter_and_non_facetable_field() {
    let dir = TempDir::new().unwrap();
    let manager = local_manager(&dir);
    let ns = NamespaceId::new("embedded-facet-errors").unwrap();
    let mut attributes = serde_json::Map::new();
    attributes.insert("section".into(), json!("warnings"));
    manager
        .upsert(
            &ns,
            vec![UpsertRow {
                id: 1,
                vector: unit_vector(0),
                vectors: None,
                text: Some("warning".into()),
                attributes,
            }],
        )
        .await
        .expect("upsert");

    let err = manager
        .facet(&ns, Some("section =".into()), &["section".into()], 10)
        .await
        .expect_err("malformed facet filter must fail");
    assert!(matches!(err, FirnflowError::InvalidRequest(_)));

    let err = manager
        .facet(&ns, None, &["vector".into()], 10)
        .await
        .expect_err("vector is not facetable");
    assert!(matches!(err, FirnflowError::InvalidRequest(_)));
}

#[tokio::test]
async fn local_fs_facet_empty_namespace_returns_empty() {
    let dir = TempDir::new().unwrap();
    let manager = local_manager(&dir);
    let ns = NamespaceId::new("embedded-facet-empty").unwrap();

    let result = manager
        .facet(&ns, None, &["id".into()], 10)
        .await
        .expect("empty facet");
    assert!(result.facets.is_empty());
}

#[tokio::test]
async fn local_fs_query_filter_zero_match_and_malformed_predicate() {
    let dir = TempDir::new().unwrap();
    let manager = local_manager(&dir);
    let ns = NamespaceId::new("embedded-filter-errors").unwrap();

    let rows: Vec<UpsertRow> = vec![(1u64, unit_vector(0)).into(), (2u64, unit_vector(1)).into()];
    manager.upsert(&ns, rows).await.expect("local upsert");

    let empty = manager
        .query(
            &ns,
            unit_vector(0),
            None,
            10,
            None,
            None,
            Some("id > 99".into()),
            false,
        )
        .await
        .expect("zero-match filtered query");
    assert!(empty.results.is_empty());

    let err = manager
        .query(
            &ns,
            unit_vector(0),
            None,
            10,
            None,
            None,
            Some("id =".into()),
            false,
        )
        .await
        .expect_err("malformed filter should fail");
    match err {
        FirnflowError::InvalidRequest(msg) => assert!(msg.contains("filter"), "{msg}"),
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
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
