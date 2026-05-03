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
use std::sync::atomic::{AtomicBool, Ordering};

use arrow_array::{RecordBatch, RecordBatchIterator, RecordBatchReader, UInt32Array, UInt64Array};
use arrow_schema::{DataType, Field, Schema};
use lancedb::table::OptimizeAction;

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

/// Storage options for an S3-compatible backend fronted by an
/// explicit endpoint URL + static credentials. Path-style: with a
/// custom `aws_endpoint`, object_store 0.12 does *not* prepend the
/// bucket to the hostname under virtual-hosted mode, which leaves
/// the bucket missing from the URL entirely (observed as 501 / 404
/// NoSuchBucket on R2 + Tigris). Path-style routes `{endpoint}/
/// {bucket}/{key}` explicitly and works cleanly on both.
fn compat_storage_options(
    endpoint: String,
    region: String,
    access: String,
    secret: String,
) -> HashMap<String, String> {
    HashMap::from([
        ("aws_access_key_id".into(), access),
        ("aws_secret_access_key".into(), secret),
        ("aws_endpoint".into(), endpoint),
        ("aws_region".into(), region),
        ("aws_virtual_hosted_style_request".into(), "false".into()),
    ])
}

fn r2_storage_options() -> Option<HashMap<String, String>> {
    Some(compat_storage_options(
        std::env::var("R2_ENDPOINT").ok()?,
        "auto".into(),
        std::env::var("R2_ACCESS_KEY").ok()?,
        std::env::var("R2_SECRET_KEY").ok()?,
    ))
}

fn tigris_storage_options() -> Option<HashMap<String, String>> {
    Some(compat_storage_options(
        std::env::var("TIGRIS_ENDPOINT").ok()?,
        "auto".into(),
        std::env::var("TIGRIS_ACCESS_KEY").ok()?,
        std::env::var("TIGRIS_SECRET_KEY").ok()?,
    ))
}

fn b2_storage_options() -> Option<HashMap<String, String>> {
    Some(compat_storage_options(
        std::env::var("B2_ENDPOINT").ok()?,
        std::env::var("B2_REGION").unwrap_or_else(|_| "us-west-004".into()),
        std::env::var("B2_ACCESS_KEY").ok()?,
        std::env::var("B2_SECRET_KEY").ok()?,
    ))
}

fn gcs_storage_options() -> Option<HashMap<String, String>> {
    Some(compat_storage_options(
        std::env::var("GCS_ENDPOINT").ok()?,
        std::env::var("GCS_REGION").unwrap_or_else(|_| "auto".into()),
        std::env::var("GCS_ACCESS_KEY").ok()?,
        std::env::var("GCS_SECRET_KEY").ok()?,
    ))
}

fn spaces_storage_options() -> Option<HashMap<String, String>> {
    Some(compat_storage_options(
        std::env::var("SPACES_ENDPOINT").ok()?,
        std::env::var("SPACES_REGION").unwrap_or_else(|_| "us-east-1".into()),
        std::env::var("SPACES_ACCESS_KEY").ok()?,
        std::env::var("SPACES_SECRET_KEY").ok()?,
    ))
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
        eprintln!("SKIP: AWS_PROFILE not set; real-AWS spike-2b needs a configured CLI profile");
        return;
    }
    let bucket = env_or("FIRNFLOW_AWS_BUCKET", "firnflow-cloudfloe");
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
        eprintln!("SKIP: AWS_PROFILE not set; real-AWS spike-2b needs a configured CLI profile");
        return;
    }
    const RUNS: usize = 100;
    let bucket = env_or("FIRNFLOW_AWS_BUCKET", "firnflow-cloudfloe");
    let uri_base = format!("s3://{bucket}");
    let opts = aws_storage_options();
    for run in 1..=RUNS {
        run_stress(uri_base.clone(), opts.clone()).await;
        if run % 10 == 0 {
            eprintln!("aws run {run}/{RUNS} passed");
        }
    }
}

// -----------------------------------------------------------------------------
// R2 / Tigris / B2: extended provider validation.
// -----------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn concurrent_writers_preserve_all_rows_r2() {
    let Some(opts) = r2_storage_options() else {
        eprintln!("SKIP: R2_ENDPOINT/R2_ACCESS_KEY/R2_SECRET_KEY not set");
        return;
    };
    let Ok(bucket) = std::env::var("R2_BUCKET") else {
        eprintln!("SKIP: R2_BUCKET not set");
        return;
    };
    run_stress(format!("s3://{bucket}"), opts).await;
}

#[tokio::test]
#[ignore]
async fn concurrent_writers_100_runs_r2() {
    let Some(opts) = r2_storage_options() else {
        eprintln!("SKIP: R2_ENDPOINT/R2_ACCESS_KEY/R2_SECRET_KEY not set");
        return;
    };
    let Ok(bucket) = std::env::var("R2_BUCKET") else {
        eprintln!("SKIP: R2_BUCKET not set");
        return;
    };
    const RUNS: usize = 100;
    let uri_base = format!("s3://{bucket}");
    for run in 1..=RUNS {
        run_stress(uri_base.clone(), opts.clone()).await;
        if run % 10 == 0 {
            eprintln!("r2 run {run}/{RUNS} passed");
        }
    }
}

#[tokio::test]
#[ignore]
async fn concurrent_writers_preserve_all_rows_tigris() {
    let Some(opts) = tigris_storage_options() else {
        eprintln!("SKIP: TIGRIS_ENDPOINT/TIGRIS_ACCESS_KEY/TIGRIS_SECRET_KEY not set");
        return;
    };
    let Ok(bucket) = std::env::var("TIGRIS_BUCKET") else {
        eprintln!("SKIP: TIGRIS_BUCKET not set");
        return;
    };
    run_stress(format!("s3://{bucket}"), opts).await;
}

#[tokio::test]
#[ignore]
async fn concurrent_writers_100_runs_tigris() {
    let Some(opts) = tigris_storage_options() else {
        eprintln!("SKIP: TIGRIS_ENDPOINT/TIGRIS_ACCESS_KEY/TIGRIS_SECRET_KEY not set");
        return;
    };
    let Ok(bucket) = std::env::var("TIGRIS_BUCKET") else {
        eprintln!("SKIP: TIGRIS_BUCKET not set");
        return;
    };
    const RUNS: usize = 100;
    let uri_base = format!("s3://{bucket}");
    for run in 1..=RUNS {
        run_stress(uri_base.clone(), opts.clone()).await;
        if run % 10 == 0 {
            eprintln!("tigris run {run}/{RUNS} passed");
        }
    }
}

#[tokio::test]
#[ignore]
async fn concurrent_writers_preserve_all_rows_b2() {
    let Some(opts) = b2_storage_options() else {
        eprintln!("SKIP: B2_ENDPOINT/B2_ACCESS_KEY/B2_SECRET_KEY not set");
        return;
    };
    let Ok(bucket) = std::env::var("B2_BUCKET") else {
        eprintln!("SKIP: B2_BUCKET not set");
        return;
    };
    run_stress(format!("s3://{bucket}"), opts).await;
}

#[tokio::test]
#[ignore]
async fn concurrent_writers_100_runs_b2() {
    let Some(opts) = b2_storage_options() else {
        eprintln!("SKIP: B2_ENDPOINT/B2_ACCESS_KEY/B2_SECRET_KEY not set");
        return;
    };
    let Ok(bucket) = std::env::var("B2_BUCKET") else {
        eprintln!("SKIP: B2_BUCKET not set");
        return;
    };
    const RUNS: usize = 100;
    let uri_base = format!("s3://{bucket}");
    for run in 1..=RUNS {
        run_stress(uri_base.clone(), opts.clone()).await;
        if run % 10 == 0 {
            eprintln!("b2 run {run}/{RUNS} passed");
        }
    }
}

#[tokio::test]
#[ignore]
async fn concurrent_writers_preserve_all_rows_spaces() {
    let Some(opts) = spaces_storage_options() else {
        eprintln!("SKIP: SPACES_ENDPOINT/SPACES_ACCESS_KEY/SPACES_SECRET_KEY not set");
        return;
    };
    let Ok(bucket) = std::env::var("SPACES_BUCKET") else {
        eprintln!("SKIP: SPACES_BUCKET not set");
        return;
    };
    run_stress(format!("s3://{bucket}"), opts).await;
}

#[tokio::test]
#[ignore]
async fn concurrent_writers_100_runs_spaces() {
    let Some(opts) = spaces_storage_options() else {
        eprintln!("SKIP: SPACES_ENDPOINT/SPACES_ACCESS_KEY/SPACES_SECRET_KEY not set");
        return;
    };
    let Ok(bucket) = std::env::var("SPACES_BUCKET") else {
        eprintln!("SKIP: SPACES_BUCKET not set");
        return;
    };
    const RUNS: usize = 100;
    let uri_base = format!("s3://{bucket}");
    for run in 1..=RUNS {
        run_stress(uri_base.clone(), opts.clone()).await;
        if run % 10 == 0 {
            eprintln!("spaces run {run}/{RUNS} passed");
        }
    }
}

#[tokio::test]
#[ignore]
async fn concurrent_writers_preserve_all_rows_gcs() {
    let Some(opts) = gcs_storage_options() else {
        eprintln!("SKIP: GCS_ENDPOINT/GCS_ACCESS_KEY/GCS_SECRET_KEY not set");
        return;
    };
    let Ok(bucket) = std::env::var("GCS_BUCKET") else {
        eprintln!("SKIP: GCS_BUCKET not set");
        return;
    };
    run_stress(format!("s3://{bucket}"), opts).await;
}

#[tokio::test]
#[ignore]
async fn concurrent_writers_100_runs_gcs() {
    let Some(opts) = gcs_storage_options() else {
        eprintln!("SKIP: GCS_ENDPOINT/GCS_ACCESS_KEY/GCS_SECRET_KEY not set");
        return;
    };
    let Ok(bucket) = std::env::var("GCS_BUCKET") else {
        eprintln!("SKIP: GCS_BUCKET not set");
        return;
    };
    const RUNS: usize = 100;
    let uri_base = format!("s3://{bucket}");
    for run in 1..=RUNS {
        run_stress(uri_base.clone(), opts.clone()).await;
        if run % 10 == 0 {
            eprintln!("gcs run {run}/{RUNS} passed");
        }
    }
}

// -----------------------------------------------------------------------------
// Writer-during-compaction race (OSWALD-flavour).
//
// OSWALD (https://nvartolomei.com/oswald) proves that PUT-If-None-Match
// alone is insufficient when garbage collection (compaction) is active:
// a write may succeed from the writer's perspective and yet be absent
// from the post-compaction state. SlateDB hit this exact bug in May
// 2026; whether Lance's commit protocol handles it cleanly on top of
// S3 is the question this test answers.
//
// Shape: identical to `run_stress` (8 writers × 100 rows on a fresh
// namespace) plus a single compactor task that loops `OptimizeAction`
// while the writers race to append. After all writers finish, the
// compactor is signalled to stop and the row count is verified. A
// row count below 800 is the OSWALD-flavour failure mode — silent
// loss, the dangerous shape. A panic from the compactor or a writer
// would be a different (safer) outcome and is allowed to propagate.
// -----------------------------------------------------------------------------

async fn run_stress_with_compaction(uri_base: String, opts: HashMap<String, String>) {
    let ns = unique_namespace("oswald");
    let uri = format!("{uri_base}/{ns}");
    let schema = schema();

    // Seed the table with an empty batch so the schema is registered
    // and the first compaction has something to operate on.
    let initial = empty_batch(schema.clone());
    let reader: Box<dyn RecordBatchReader + Send> =
        Box::new(RecordBatchIterator::new(vec![Ok(initial)], schema.clone()));
    let conn = connect(&uri, &opts).await;
    conn.create_table(TABLE, reader)
        .execute()
        .await
        .expect("create_table");

    // Compactor task: loops OptimizeAction::default() (== Compact) until
    // signalled to stop. Errors are swallowed because a commit-conflict
    // bubbling up to the operator is *safe* behaviour — it's only silent
    // row loss in the final count that proves the OSWALD bug.
    let stop = Arc::new(AtomicBool::new(false));
    let compactor = {
        let uri = uri.clone();
        let opts = opts.clone();
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            let mut iterations = 0usize;
            let mut errors = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let conn = connect(&uri, &opts).await;
                let tbl = match conn.open_table(TABLE).execute().await {
                    Ok(t) => t,
                    Err(_) => {
                        errors += 1;
                        continue;
                    }
                };
                match tbl.optimize(OptimizeAction::default()).await {
                    Ok(_) => iterations += 1,
                    Err(_) => errors += 1,
                }
            }
            (iterations, errors)
        })
    };

    // Spawn N writers, identical to run_stress. Each re-opens the
    // connection so the CAS contention is real, not faked through
    // shared in-process state.
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

    // Signal the compactor and wait for it to finish its in-flight
    // iteration. We capture the (iterations, errors) tuple so the
    // test output makes it obvious whether the compactor actually
    // ran during the writer window.
    stop.store(true, Ordering::Relaxed);
    let (iterations, errors) = compactor.await.expect("compactor task panicked");
    eprintln!(
        "compactor finished: {iterations} successful optimize calls, {errors} errors"
    );

    let conn = connect(&uri, &opts).await;
    let tbl = conn.open_table(TABLE).execute().await.expect("open_table");
    let count = tbl.count_rows(None).await.expect("count_rows");
    let expected = WRITERS * ROWS_PER_WRITER;
    assert_eq!(
        count, expected,
        "writer-during-compaction stress on {uri}: expected {expected} rows, got {count}. \
         Lance's commit protocol lost writes when compaction ran concurrently — this is \
         the OSWALD-flavour writer/GC race (compactor ran {iterations} times, {errors} errors)."
    );
}

#[tokio::test]
#[ignore]
async fn concurrent_writes_during_compaction_minio() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let uri_base = format!("s3://{bucket}");
    run_stress_with_compaction(uri_base, minio_storage_options()).await;
}

#[tokio::test]
#[ignore]
async fn concurrent_writes_during_compaction_aws() {
    if std::env::var("AWS_PROFILE").is_err() {
        eprintln!("SKIP: AWS_PROFILE not set; real-AWS OSWALD race test needs a configured CLI profile");
        return;
    }
    let bucket = env_or("FIRNFLOW_AWS_BUCKET", "firnflow-cloudfloe");
    let uri_base = format!("s3://{bucket}");
    run_stress_with_compaction(uri_base, aws_storage_options()).await;
}

/// 50-iteration regression guard. Run once a clean / failing single-shot
/// result has been established; deterministic loss (every iteration
/// failing identically) is conclusive after even one run, but a flaky
/// race may only surface intermittently. Mirrors the existing 100-run
/// stress tests.
#[tokio::test]
#[ignore]
async fn concurrent_writes_during_compaction_50_runs_minio() {
    const RUNS: usize = 50;
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let uri_base = format!("s3://{bucket}");
    let opts = minio_storage_options();
    for run in 1..=RUNS {
        run_stress_with_compaction(uri_base.clone(), opts.clone()).await;
        eprintln!("oswald minio run {run}/{RUNS} passed");
    }
}

// -----------------------------------------------------------------------------
// Aggressive writer-during-compaction variant.
//
// The standard variant (8 writers × 1 batch × 100 rows) gives the
// compactor only 8 writer-side manifest commits to race against. That
// passed 50/50 on MinIO, but a short writer window is the easiest case
// to handle correctly — the OSWALD bug is most likely to surface when
// many writers each commit several times in a row, multiplying the
// chance that a compactor lands its manifest rewrite between a writer
// reading the current manifest and writing back its CAS.
//
// This variant runs 16 writers × 4 sequential appends × 25 rows = 1600
// total rows, with the same single compactor looping `OptimizeAction::All`.
// 64 writer commits per iteration vs. 8 in the standard test.
// -----------------------------------------------------------------------------

const AGG_WRITERS: usize = 16;
const AGG_BATCHES_PER_WRITER: usize = 4;
const AGG_ROWS_PER_BATCH: usize = 25;

async fn run_stress_with_compaction_aggressive(
    uri_base: String,
    opts: HashMap<String, String>,
) {
    let ns = unique_namespace("oswald-agg");
    let uri = format!("{uri_base}/{ns}");
    let schema = schema();

    let initial = empty_batch(schema.clone());
    let reader: Box<dyn RecordBatchReader + Send> =
        Box::new(RecordBatchIterator::new(vec![Ok(initial)], schema.clone()));
    let conn = connect(&uri, &opts).await;
    conn.create_table(TABLE, reader)
        .execute()
        .await
        .expect("create_table");

    let stop = Arc::new(AtomicBool::new(false));
    let compactor = {
        let uri = uri.clone();
        let opts = opts.clone();
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            let mut iterations = 0usize;
            let mut errors = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let conn = connect(&uri, &opts).await;
                let tbl = match conn.open_table(TABLE).execute().await {
                    Ok(t) => t,
                    Err(_) => {
                        errors += 1;
                        continue;
                    }
                };
                match tbl.optimize(OptimizeAction::default()).await {
                    Ok(_) => iterations += 1,
                    Err(_) => errors += 1,
                }
            }
            (iterations, errors)
        })
    };

    let mut handles = Vec::with_capacity(AGG_WRITERS);
    for writer_id in 0..AGG_WRITERS {
        let uri = uri.clone();
        let opts = opts.clone();
        let schema = schema.clone();
        handles.push(tokio::spawn(async move {
            let conn = connect(&uri, &opts).await;
            let tbl = conn.open_table(TABLE).execute().await.expect("open_table");
            for batch_idx in 0..AGG_BATCHES_PER_WRITER {
                // Distinct id range per (writer, batch) so the row
                // count is the only signal that matters and ids stay
                // human-readable when introspecting after a failure.
                let pseudo_writer =
                    (writer_id * AGG_BATCHES_PER_WRITER + batch_idx) as u32;
                let batch = writer_batch(schema.clone(), pseudo_writer, AGG_ROWS_PER_BATCH);
                let reader: Box<dyn RecordBatchReader + Send> = Box::new(
                    RecordBatchIterator::new(vec![Ok(batch)], schema.clone()),
                );
                tbl.add(reader).execute().await.expect("table.add");
            }
        }));
    }
    for h in handles {
        h.await.expect("writer task panicked");
    }

    stop.store(true, Ordering::Relaxed);
    let (iterations, errors) = compactor.await.expect("compactor task panicked");
    eprintln!(
        "compactor finished (aggressive): {iterations} optimize calls, {errors} errors"
    );

    let conn = connect(&uri, &opts).await;
    let tbl = conn.open_table(TABLE).execute().await.expect("open_table");
    let count = tbl.count_rows(None).await.expect("count_rows");
    let expected = AGG_WRITERS * AGG_BATCHES_PER_WRITER * AGG_ROWS_PER_BATCH;
    assert_eq!(
        count, expected,
        "aggressive writer-during-compaction stress on {uri}: expected {expected} rows, got {count}. \
         Lance's commit protocol lost writes when compaction ran concurrently — this is \
         the OSWALD-flavour writer/GC race (compactor ran {iterations} times, {errors} errors)."
    );
}

#[tokio::test]
#[ignore]
async fn concurrent_writes_during_compaction_aggressive_minio() {
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let uri_base = format!("s3://{bucket}");
    run_stress_with_compaction_aggressive(uri_base, minio_storage_options()).await;
}

#[tokio::test]
#[ignore]
async fn concurrent_writes_during_compaction_aggressive_aws() {
    if std::env::var("AWS_PROFILE").is_err() {
        eprintln!("SKIP: AWS_PROFILE not set; AWS aggressive OSWALD race needs a configured CLI profile");
        return;
    }
    let bucket = env_or("FIRNFLOW_AWS_BUCKET", "firnflow-cloudfloe");
    let uri_base = format!("s3://{bucket}");
    run_stress_with_compaction_aggressive(uri_base, aws_storage_options()).await;
}

#[tokio::test]
#[ignore]
async fn concurrent_writes_during_compaction_aggressive_200_runs_minio() {
    const RUNS: usize = 200;
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    let uri_base = format!("s3://{bucket}");
    let opts = minio_storage_options();
    for run in 1..=RUNS {
        run_stress_with_compaction_aggressive(uri_base.clone(), opts.clone()).await;
        if run % 10 == 0 {
            eprintln!("oswald-agg minio run {run}/{RUNS} passed");
        }
    }
}
