//! Live object-cache smoke test (issue #51), S3-gated `#[ignore]`.
//!
//! Exercises the cache through Firn's real `NamespaceManager` + the session wiring
//! (`with_object_cache_session`), against real object storage, and proves the three merge gates:
//!   1. a repeated (warm) query issues fewer S3 GETs than its first (cold) execution, and produces
//!      cache hits;
//!   2. namespace delete + recreate serves the NEW data — cached bytes from the old table are never
//!      served (manifests aren't cached; data/index are uniquely named);
//!   3. cached payload on disk stays within the configured byte cap across a simulated restart.
//!
//! Runs against AWS S3 or any S3-compatible endpoint (e.g. MinIO). Against MinIO, set
//! `FIRNFLOW_S3_ENDPOINT` (path-style + http are enabled automatically), mirroring the repo's
//! other S3-gated tests.
//!
//! ```text
//! # AWS S3
//! FIRNFLOW_S3_BUCKET=<bucket> FIRNFLOW_S3_ACCESS_KEY=<id> FIRNFLOW_S3_SECRET_KEY=<secret> \
//! FIRNFLOW_S3_REGION=eu-west-1 \
//!   ./scripts/cargo test -p firnflow-core --test object_cache_s3_smoke -- --ignored --nocapture
//!
//! # MinIO (docker compose up -d minio minio-init)
//! FIRNFLOW_S3_BUCKET=firnflow-test FIRNFLOW_S3_ACCESS_KEY=minioadmin \
//! FIRNFLOW_S3_SECRET_KEY=minioadmin FIRNFLOW_S3_ENDPOINT=http://127.0.0.1:9000 \
//!   ./scripts/cargo test -p firnflow-core --test object_cache_s3_smoke -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use firnflow_core::metrics::test_metrics;
use firnflow_core::object_cache::{build_cached_session, ObjectCacheConfig, ObjectCacheMetrics};
use firnflow_core::{NamespaceId, NamespaceManager, StorageRoot, UpsertRow};

const DIM: usize = 128;
const ROWS: u64 = 10_000;

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("set {k} for the live smoke test"))
}
fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

fn s3_options() -> HashMap<String, String> {
    let mut o = HashMap::from([
        ("aws_access_key_id".into(), env("FIRNFLOW_S3_ACCESS_KEY")),
        (
            "aws_secret_access_key".into(),
            env("FIRNFLOW_S3_SECRET_KEY"),
        ),
        (
            "aws_region".into(),
            env_or("FIRNFLOW_S3_REGION", "eu-west-1"),
        ),
    ]);
    // Optional S3-compatible endpoint (MinIO et al). When set, use path-style addressing over
    // http, matching the repo's existing MinIO-gated tests. Absent → default AWS (virtual-hosted).
    if let Ok(endpoint) = std::env::var("FIRNFLOW_S3_ENDPOINT") {
        o.insert("aws_endpoint".into(), endpoint);
        o.insert("allow_http".into(), "true".into());
        o.insert("aws_virtual_hosted_style_request".into(), "false".into());
    }
    o
}

fn vec_for(seed: u64) -> Vec<f32> {
    (0..DIM as u64)
        .map(|i| ((seed.wrapping_mul(2_654_435_761).wrapping_add(i)) % 1000) as f32 / 1000.0)
        .collect()
}

fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{n}")
}

/// Bytes of cached object data on disk — the quantity the byte cap governs. Excludes the small
/// `.sz` size sidecars and any in-flight `.tmp` files, which are auxiliary, not cached payload.
fn payload_bytes(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            match e.metadata() {
                Ok(m) if m.is_dir() => total += payload_bytes(&e.path()),
                Ok(m) => {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    if !name.ends_with(".sz") && !name.ends_with(".tmp") {
                        total += m.len();
                    }
                }
                Err(_) => {}
            }
        }
    }
    total
}

#[tokio::test]
#[ignore]
async fn object_cache_live_s3_gates() {
    let bucket = env("FIRNFLOW_S3_BUCKET");
    let opts = s3_options();
    // TempDir removes the cache directory on drop, including on a panic during unwind.
    let cache_dir = tempfile::tempdir().expect("temp cache dir");
    let ns = NamespaceId::new(unique("objcache")).unwrap();

    let result = run_gates(&bucket, &opts, cache_dir.path(), &ns).await;

    // Always clean up the namespace in the bucket, pass or fail, before surfacing the outcome.
    let _ = NamespaceManager::new(
        StorageRoot::s3_bucket(&bucket).unwrap(),
        s3_options(),
        test_metrics(),
    )
    .delete(&ns)
    .await;

    result.expect("object-cache live S3 gates");
    println!("ALL GATES PASSED");
}

async fn run_gates(
    bucket: &str,
    opts: &HashMap<String, String>,
    cache_root: &Path,
    ns: &NamespaceId,
) -> Result<(), String> {
    let root = || StorageRoot::s3_bucket(bucket).map_err(|e| e.to_string());

    // ---------- gate 1: warm query drops S3 GETs ----------
    let cfg = ObjectCacheConfig::new(cache_root.to_path_buf(), 5 * 1024 * 1024 * 1024);
    let metrics = Arc::new(ObjectCacheMetrics::unregistered());
    let session = build_cached_session(&cfg, metrics.clone());
    let manager = NamespaceManager::new(root()?, opts.clone(), test_metrics())
        .with_object_cache_session(session);

    let rows: Vec<UpsertRow> = (0..ROWS).map(|i| (i, vec_for(i)).into()).collect();
    manager
        .upsert(ns, rows)
        .await
        .map_err(|e| format!("upsert: {e}"))?;

    let q = vec_for(42);
    let (g0, h0, ..) = metrics.snapshot();
    let cold = manager
        .query(ns, q.clone(), None, 10, None, None, None, false)
        .await
        .map_err(|e| format!("cold query: {e}"))?;
    let (g1, h1, ..) = metrics.snapshot();
    let warm = manager
        .query(ns, q.clone(), None, 10, None, None, None, false)
        .await
        .map_err(|e| format!("warm query: {e}"))?;
    let (g2, h2, ..) = metrics.snapshot();

    let (cold_gets, warm_gets) = (g1 - g0, g2 - g1);
    println!(
        "GATE 1  cold_S3_GETs={cold_gets}  warm_S3_GETs={warm_gets}  cold_hits={}  warm_hits={}",
        h1 - h0,
        h2 - h1
    );
    if cold.results.len() != 10 {
        return Err(format!(
            "cold query returned {} results, want 10",
            cold.results.len()
        ));
    }
    if warm.results.len() != 10 {
        return Err(format!(
            "warm query returned {} results, want 10",
            warm.results.len()
        ));
    }
    if cold_gets == 0 {
        return Err("cold query made no S3 GETs".into());
    }
    if warm_gets >= cold_gets {
        return Err(format!(
            "warm GETs {warm_gets} not fewer than cold {cold_gets}"
        ));
    }
    if h2 <= h1 {
        return Err("warm query produced no cache hits".into());
    }

    // ---------- gate 2: delete + recreate serves NEW data (no stale) ----------
    manager
        .delete(ns)
        .await
        .map_err(|e| format!("delete: {e}"))?;
    let marker = 987_654_u64;
    let mut mvec = vec![0.0_f32; DIM];
    mvec[0] = 9.0; // far from the old normalized vectors
    manager
        .upsert(ns, vec![(marker, mvec.clone()).into()])
        .await
        .map_err(|e| format!("recreate upsert: {e}"))?;
    let after = manager
        .query(ns, mvec, None, 1, None, None, None, false)
        .await
        .map_err(|e| format!("post-recreate query: {e}"))?;
    let top = after.results.first().map(|r| r.id);
    println!("GATE 2  post-recreate top id={top:?}");
    if top != Some(marker) {
        return Err(format!(
            "recreate served stale data: top id {top:?}, want {marker}"
        ));
    }

    let payload_before = payload_bytes(cache_root);
    drop(manager);
    drop(metrics);

    // ---------- gate 3: byte cap honoured across a simulated restart ----------
    // Rebuild over the SAME cache dir with a tiny cap; opening the store on the next query triggers
    // the per-store startup scan, which evicts cached payload down to the cap.
    let small_cap = 1024 * 1024; // 1 MiB — below the gate-1 working set
    let cfg2 = ObjectCacheConfig::new(cache_root.to_path_buf(), small_cap);
    let session2 = build_cached_session(&cfg2, Arc::new(ObjectCacheMetrics::unregistered()));
    let manager2 = NamespaceManager::new(root()?, opts.clone(), test_metrics())
        .with_object_cache_session(session2);
    manager2
        .query(ns, vec_for(7), None, 1, None, None, None, false)
        .await
        .map_err(|e| format!("post-restart query: {e}"))?;
    let payload_after = payload_bytes(cache_root);
    println!(
        "GATE 3  payload bytes before restart={payload_before}  after tiny-cap restart={payload_after}  (cap={small_cap})"
    );
    // The gate only proves eviction if the pre-restart set actually exceeded the cap.
    if payload_before <= small_cap {
        return Err(format!(
            "gate 3 inconclusive: pre-restart payload {payload_before} did not exceed cap {small_cap}, so eviction was not exercised"
        ));
    }
    if payload_after > small_cap {
        return Err(format!(
            "cached payload {payload_after} exceeds cap {small_cap} after restart"
        ));
    }

    Ok(())
}
