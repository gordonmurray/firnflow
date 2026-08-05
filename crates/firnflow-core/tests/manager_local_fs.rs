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
            attributes: Default::default(),
        },
        UpsertRow {
            id: 2,
            vector: unit_vector(1),
            vectors: None,
            text: Some("a lazy dog sleeps".into()),
            attributes: Default::default(),
        },
        UpsertRow {
            id: 3,
            vector: unit_vector(2),
            vectors: None,
            text: Some("the fox runs fast".into()),
            attributes: Default::default(),
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
            attributes: Default::default(),
        },
        UpsertRow {
            id: 2,
            vector: unit_vector(1),
            vectors: None,
            text: Some("fox dosing".into()),
            attributes: Default::default(),
        },
        UpsertRow {
            id: 3,
            vector: unit_vector(2),
            vectors: None,
            text: Some("dog warning".into()),
            attributes: Default::default(),
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

#[tokio::test]
async fn local_fs_text_query_without_fts_index_is_a_bad_request() {
    // Regression for #103. Firn sends `text` to Lance without naming a
    // column, so Lance resolves the target from the FTS-indexed columns
    // and fails plan building when there are none. That reached callers
    // as a 500, which reads as a storage fault and invites a retry that
    // can never succeed.
    let dir = TempDir::new().unwrap();
    let manager = local_manager(&dir);
    let ns = NamespaceId::new("embedded-fts-unindexed").unwrap();

    let rows: Vec<UpsertRow> = vec![
        UpsertRow {
            id: 1,
            vector: unit_vector(0),
            vectors: None,
            text: Some("the quick brown fox".into()),
            attributes: Default::default(),
        },
        UpsertRow {
            id: 2,
            vector: unit_vector(1),
            vectors: None,
            text: Some("a lazy dog sleeps".into()),
            attributes: Default::default(),
        },
    ];
    manager.upsert(&ns, rows).await.expect("local upsert");

    // Deliberately no `create_fts_index` call.

    let expect_missing_index = |err: FirnflowError, case: &str| match err {
        FirnflowError::InvalidRequest(msg) => {
            assert!(
                msg.contains("BM25 index") && msg.contains("/fts-index"),
                "{case}: error must name the index and how to build it, got {msg}"
            );
        }
        other => panic!("{case}: expected InvalidRequest, got {other:?}"),
    };

    // FTS-only.
    let err = manager
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
        .expect_err("FTS-only without an index must fail");
    expect_missing_index(err, "fts-only");

    // Hybrid. The vector leg would succeed on its own, so this proves
    // the error is not swallowed by the fusion path.
    let err = manager
        .query(
            &ns,
            unit_vector(0),
            None,
            10,
            None,
            Some("fox".into()),
            None,
            false,
        )
        .await
        .expect_err("hybrid without an FTS index must fail");
    expect_missing_index(err, "hybrid");

    // Filtered text query. This one already returned InvalidRequest
    // before the fix, but blamed the `filter` for a fault that has
    // nothing to do with it, so assert on the message, not the variant.
    let err = manager
        .query(
            &ns,
            Vec::new(),
            None,
            10,
            None,
            Some("fox".into()),
            Some("id > 0".into()),
            false,
        )
        .await
        .expect_err("filtered FTS without an index must fail");
    expect_missing_index(err, "filtered fts");

    // Vector-only is unaffected. The probe must not fire on a query
    // that carries no text.
    let results = manager
        .query(&ns, unit_vector(0), None, 10, None, None, None, false)
        .await
        .expect("vector-only query must still succeed without an FTS index");
    assert_eq!(
        results.results.len(),
        2,
        "vector-only must return both rows"
    );

    // Building the index clears all three failures: the probe reports
    // what is actually true rather than rejecting every text query.
    manager
        .create_fts_index(&ns)
        .await
        .expect("create fts index");
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
        .expect("FTS query must succeed once the index exists");
    let ids: Vec<u64> = results.results.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![1], "'fox' should match only row 1, got {ids:?}");
}

#[tokio::test]
async fn local_fs_text_query_on_unwritten_namespace_is_empty_not_an_error() {
    // The other half of #103: a namespace that has never been written
    // has no table to index, so a text query against it returns an
    // empty result set rather than the missing-index error. That is why
    // a fresh deployment looks healthy right up until the first write.
    let dir = TempDir::new().unwrap();
    let manager = local_manager(&dir);
    let ns = NamespaceId::new("embedded-fts-unwritten").unwrap();

    // Every query shape, since that is what the docs promise. The
    // vector legs matter as much as the text ones, because an unwritten
    // namespace has no dimension to validate a query vector against.
    for (case, vector, text) in [
        ("fts-only", Vec::new(), Some("fox".to_string())),
        ("hybrid", unit_vector(0), Some("fox".to_string())),
        ("vector-only", unit_vector(0), None),
    ] {
        let results = manager
            .query(&ns, vector, None, 10, None, text, None, false)
            .await
            .unwrap_or_else(|e| panic!("{case} on an unwritten namespace must not error: {e}"));
        assert!(
            results.results.is_empty(),
            "{case}: unwritten namespace must return no hits"
        );
    }
}

#[tokio::test]
async fn local_fs_fts_index_covers_rows_written_after_the_build() {
    // Raised alongside #103, and worth locking down rather than taking
    // on trust: the BTree on `id` only covers rows present at build
    // time until a compaction folds the rest in, so it is not obvious
    // the inverted index behaves differently. It does: Lance scans the
    // fragments the index does not cover and merges the scores, so a
    // term that appears only in a post-build row is still found.
    let dir = TempDir::new().unwrap();
    let manager = local_manager(&dir);
    let ns = NamespaceId::new("embedded-fts-post-build").unwrap();

    manager
        .upsert(
            &ns,
            vec![UpsertRow {
                id: 1,
                vector: unit_vector(0),
                vectors: None,
                text: Some("the quick brown fox".into()),
                attributes: Default::default(),
            }],
        )
        .await
        .expect("first upsert");
    manager
        .create_fts_index(&ns)
        .await
        .expect("create fts index");

    manager
        .upsert(
            &ns,
            vec![UpsertRow {
                id: 2,
                vector: unit_vector(1),
                vectors: None,
                text: Some("a solitary aardvark".into()),
                attributes: Default::default(),
            }],
        )
        .await
        .expect("post-build upsert");

    let results = manager
        .query(
            &ns,
            Vec::new(),
            None,
            10,
            None,
            Some("aardvark".into()),
            None,
            false,
        )
        .await
        .expect("fts query");
    let ids: Vec<u64> = results.results.iter().map(|r| r.id).collect();
    assert_eq!(
        ids,
        vec![2],
        "a term only in the post-build row must still be found, got {ids:?}"
    );
}
