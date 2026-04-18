//! Integration test for `NamespaceManager::list` (issue #22).
//!
//! Appends several batches of rows to a fresh namespace with small
//! sleeps between them so each batch gets a distinct server-side
//! `_ingested_at` timestamp. Pages through the namespace with a
//! small `limit` so pagination boundaries are forced to fall inside
//! and across batches, then asserts:
//!
//! - every id 0..N shows up exactly once,
//! - the concatenated page stream is strictly ordered by
//!   `(_ingested_at DESC, id DESC)` (in-batch ties are broken by id),
//! - the final page returns `next_cursor: None`.
//!
//! Gated `#[ignore]`: needs MinIO up.
//!
//! ```text
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-core --test manager_list \
//!     -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;

use firnflow_core::metrics::test_metrics;
use firnflow_core::{
    decode_list_cursor, ListOrder, ListRow, NamespaceId, NamespaceManager, UpsertRow,
};

const DIM: usize = 4;
const BATCHES: usize = 5;
const ROWS_PER_BATCH: usize = 10;
const PAGE_LIMIT: usize = 7;

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

fn unit_vector(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    v[axis % DIM] = 1.0;
    v
}

#[tokio::test]
#[ignore]
async fn list_paginates_in_strict_ingest_order() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let manager = NamespaceManager::new(bucket, minio_options(), test_metrics());
    let ns = NamespaceId::new(unique_namespace("issue22")).unwrap();

    // Seed `BATCHES` separated batches so each batch gets a distinct
    // `_ingested_at` timestamp. The sleep must exceed the clock
    // resolution we stamp at (microseconds) — 5ms is more than enough
    // while keeping the test quick.
    let mut expected_ids: Vec<u64> = Vec::new();
    for batch_idx in 0..BATCHES {
        let base = (batch_idx * ROWS_PER_BATCH) as u64;
        let rows: Vec<UpsertRow> = (0..ROWS_PER_BATCH)
            .map(|i| (base + i as u64, unit_vector(i)).into())
            .collect();
        for row in &rows {
            expected_ids.push(row.id);
        }
        manager.upsert(&ns, rows).await.expect("upsert batch");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let total = expected_ids.len();

    // Page through the namespace in descending order.
    let mut collected: Vec<ListRow> = Vec::new();
    let mut cursor: Option<(i64, u64)> = None;
    loop {
        let page = manager
            .list(&ns, PAGE_LIMIT, ListOrder::Desc, cursor)
            .await
            .expect("list page");
        assert!(
            page.rows.len() <= PAGE_LIMIT,
            "page returned {} rows, expected <= {PAGE_LIMIT}",
            page.rows.len()
        );
        collected.extend(page.rows);
        match page.next_cursor {
            Some(c) => {
                cursor = Some(decode_list_cursor(&c).expect("decode returned cursor"));
            }
            None => break,
        }
    }

    assert_eq!(
        collected.len(),
        total,
        "paginated total ({}) != upserted total ({total})",
        collected.len()
    );
    let seen: HashSet<u64> = collected.iter().map(|r| r.id).collect();
    assert_eq!(
        seen.len(),
        total,
        "ids collected across pages must be unique"
    );
    for id in &expected_ids {
        assert!(seen.contains(id), "id {id} missing from list output");
    }

    // Strict descending order on (ingested_at, id). In-batch rows
    // share an `_ingested_at`; the secondary id-desc tiebreak must
    // keep them stable and monotonically decreasing.
    for window in collected.windows(2) {
        let (a, b) = (&window[0], &window[1]);
        let a_key = (a.ingested_at_micros, a.id);
        let b_key = (b.ingested_at_micros, b.id);
        assert!(
            a_key > b_key,
            "ordering violated at page boundary: {a_key:?} should be > {b_key:?}"
        );
    }

    // Ascending pass — verify the endpoint's symmetric behaviour.
    let mut asc_collected: Vec<ListRow> = Vec::new();
    let mut cursor: Option<(i64, u64)> = None;
    loop {
        let page = manager
            .list(&ns, PAGE_LIMIT, ListOrder::Asc, cursor)
            .await
            .expect("list page (asc)");
        asc_collected.extend(page.rows);
        match page.next_cursor {
            Some(c) => cursor = Some(decode_list_cursor(&c).unwrap()),
            None => break,
        }
    }
    assert_eq!(asc_collected.len(), total);
    for window in asc_collected.windows(2) {
        let a_key = (window[0].ingested_at_micros, window[0].id);
        let b_key = (window[1].ingested_at_micros, window[1].id);
        assert!(
            a_key < b_key,
            "ascending order violated: {a_key:?} must be < {b_key:?}"
        );
    }
}

#[tokio::test]
#[ignore]
async fn list_empty_namespace_returns_empty_page() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let manager = NamespaceManager::new(bucket, minio_options(), test_metrics());
    let ns = NamespaceId::new(unique_namespace("issue22-empty")).unwrap();

    let page = manager
        .list(&ns, 50, ListOrder::Desc, None)
        .await
        .expect("list on fresh namespace must not error");
    assert!(page.rows.is_empty());
    assert!(page.next_cursor.is_none());
}
