//! Spike-2b: LanceDB concurrent-writer stress test.
//!
//! CLAUDE.md § "Known hard problems" 2:
//! > N writers to the same namespace simultaneously, each appending
//! > M rows. After all writes complete, query the full table and
//! > assert row count == N * M.
//!
//! Gated `#[ignore]`: the test talks to MinIO or real AWS S3, both of
//! which are out-of-process. Run with:
//!
//! ```text
//! # MinIO
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-core --test lance_concurrent_writes \
//!     concurrent_writers_preserve_all_rows_minio -- --ignored --nocapture
//!
//! # AWS S3
//! AWS_PROFILE=cloudfloe ./scripts/cargo test -p firnflow-core \
//!     --test lance_concurrent_writes \
//!     concurrent_writers_preserve_all_rows_aws -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::{RecordBatch, RecordBatchIterator, RecordBatchReader, UInt32Array, UInt64Array};
use arrow_schema::{DataType, Field, Schema};

const WRITERS: usize = 8;
const ROWS_PER_WRITER: usize = 100;
const TABLE: &str = "data";

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

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt64, false),
        Field::new("writer", DataType::UInt32, false),
    ]))
}

fn empty_batch(schema: Arc<Schema>) -> RecordBatch {
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(UInt64Array::from(Vec::<u64>::new())),
            Arc::new(UInt32Array::from(Vec::<u32>::new())),
        ],
    )
    .unwrap()
}

fn writer_batch(schema: Arc<Schema>, writer: u32, rows: usize) -> RecordBatch {
    let base = u64::from(writer) * rows as u64;
    let ids: Vec<u64> = (0..rows as u64).map(|i| base + i).collect();
    let writers: Vec<u32> = std::iter::repeat_n(writer, rows).collect();
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(UInt32Array::from(writers)),
        ],
    )
    .unwrap()
}

fn minio_storage_options() -> HashMap<String, String> {
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

fn aws_storage_options() -> HashMap<String, String> {
    HashMap::from([("aws_region".into(), env_or("AWS_REGION", "eu-west-1"))])
}

async fn connect(uri: &str, opts: &HashMap<String, String>) -> lancedb::Connection {
    lancedb::connect(uri)
        .storage_options(opts.clone())
        .execute()
        .await
        .expect("lancedb connect")
}

async fn run_stress(uri_base: String, opts: HashMap<String, String>) {
    let ns = unique_namespace("spike2b");
    let uri = format!("{uri_base}/{ns}");
    let schema = schema();

    // Seed the table with an empty batch so the schema is registered
    // before any writer opens it.
    let initial = empty_batch(schema.clone());
    let reader: Box<dyn RecordBatchReader + Send> =
        Box::new(RecordBatchIterator::new(vec![Ok(initial)], schema.clone()));
    let conn = connect(&uri, &opts).await;
    conn.create_table(TABLE, reader)
        .execute()
        .await
        .expect("create_table");

    // Spawn N writers. Each re-opens the connection so we exercise
    // real concurrent CAS writes, not shared process state.
    let mut handles = Vec::with_capacity(WRITERS);
    for writer_id in 0..WRITERS {
        let uri = uri.clone();
        let opts = opts.clone();
        let schema = schema.clone();
        handles.push(tokio::spawn(async move {
            let conn = connect(&uri, &opts).await;
            let tbl = conn.open_table(TABLE).execute().await.expect("open_table");
            let batch = writer_batch(schema.clone(), writer_id as u32, ROWS_PER_WRITER);
            let reader: Box<dyn RecordBatchReader + Send> =
                Box::new(RecordBatchIterator::new(vec![Ok(batch)], schema));
            tbl.add(reader).execute().await.expect("table.add");
        }));
    }
    for h in handles {
        h.await.expect("writer task panicked");
    }

    // Verify: row count must equal every row every writer claimed to
    // have added. Anything less indicates a lost-update bug in Lance's
    // CAS-based WAL on this backend.
    let conn = connect(&uri, &opts).await;
    let tbl = conn.open_table(TABLE).execute().await.expect("open_table");
    let count = tbl.count_rows(None).await.expect("count_rows");
    let expected = WRITERS * ROWS_PER_WRITER;
    assert_eq!(
        count, expected,
        "concurrent-write stress on {uri}: expected {expected} rows, got {count}. \
         Lance's CAS-based WAL lost writes on this backend; do not ship."
    );
}

#[tokio::test]
#[ignore]
async fn concurrent_writers_preserve_all_rows_minio() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let uri_base = format!("s3://{bucket}");
    run_stress(uri_base, minio_storage_options()).await;
}

#[tokio::test]
#[ignore]
async fn concurrent_writers_preserve_all_rows_aws() {
    if std::env::var("AWS_PROFILE").is_err() {
        eprintln!("SKIP: AWS_PROFILE not set — real-AWS spike-2b needs a configured CLI profile");
        return;
    }
    let bucket = env_or("FIRNFLOW_AWS_BUCKET", "lakestream-spike2-cloudfloe");
    let uri_base = format!("s3://{bucket}");
    run_stress(uri_base, aws_storage_options()).await;
}

/// CLAUDE.md § "Known hard problems" 2 demands 100 passing runs as
/// the definition of done for this spike. Each iteration uses a fresh
/// namespace; total S3 footprint is bounded by (iterations × 800 rows).
#[tokio::test]
#[ignore]
async fn concurrent_writers_100_runs_minio() {
    const RUNS: usize = 100;
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let uri_base = format!("s3://{bucket}");
    let opts = minio_storage_options();
    for run in 1..=RUNS {
        run_stress(uri_base.clone(), opts.clone()).await;
        if run % 10 == 0 {
            eprintln!("minio run {run}/{RUNS} passed");
        }
    }
}

#[tokio::test]
#[ignore]
async fn concurrent_writers_100_runs_aws() {
    if std::env::var("AWS_PROFILE").is_err() {
        eprintln!("SKIP: AWS_PROFILE not set — real-AWS spike-2b needs a configured CLI profile");
        return;
    }
    const RUNS: usize = 100;
    let bucket = env_or("FIRNFLOW_AWS_BUCKET", "lakestream-spike2-cloudfloe");
    let uri_base = format!("s3://{bucket}");
    let opts = aws_storage_options();
    for run in 1..=RUNS {
        run_stress(uri_base.clone(), opts.clone()).await;
        if run % 10 == 0 {
            eprintln!("aws run {run}/{RUNS} passed");
        }
    }
}
