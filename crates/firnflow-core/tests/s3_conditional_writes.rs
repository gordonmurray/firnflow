//! Conditional-write pre-flight.
//!
//! Verifies that the S3 backend honours `If-None-Match: *` on
//! PutObject before we trust Lance's CAS-based WAL on top of it.
//! This check is required: a backend that silently ignores the
//! precondition will pass Lance's row-count assertion at low contention
//! and fail in production.
//!
//! GCS gets two extra tests at the bottom of this file because its
//! S3-interop endpoint silently drops `If-None-Match: *`. Those tests
//! use the native `object_store::gcp` client with `PutMode::Create` —
//! the same code path that future native-GCS Firn support will rely
//! on — and prove (a) the precondition fires sequentially and (b) it
//! survives a contended-key race. Distinct-key stress is not a CAS
//! test — every writer succeeds because there is no contention.
//!
//! All tests are `#[ignore]`'d because they talk to out-of-process
//! services. Run with:
//!
//! ```text
//! # MinIO (via docker compose)
//! docker compose up -d minio minio-init
//! ./scripts/cargo test -p firnflow-core --test s3_conditional_writes \
//!     put_object_with_if_none_match_rejects_second_write_minio -- --ignored --nocapture
//!
//! # Real AWS S3 (needs an AWS CLI profile and a reachable bucket)
//! AWS_PROFILE=cloudfloe ./scripts/cargo test -p firnflow-core \
//!     --test s3_conditional_writes \
//!     put_object_with_if_none_match_rejects_second_write_aws -- --ignored --nocapture
//!
//! # GCS native conditional-write verification (needs a service-account
//! # JSON with Storage Object Admin on the test bucket)
//! GOOGLE_APPLICATION_CREDENTIALS=/path/to/key.json \
//! GCS_BUCKET=firn-gcs-bucket \
//! ./scripts/cargo test -p firnflow-core --test s3_conditional_writes \
//!     gcs_native -- --ignored --nocapture
//! ```

use std::sync::Arc;

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::{Client, Config};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn unique_key(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}/{nanos}")
}

/// MinIO client: explicit credentials, HTTP endpoint, path-style.
async fn minio_client() -> Client {
    let endpoint = env_or("FIRNFLOW_S3_ENDPOINT", "http://127.0.0.1:9000");
    let access = env_or("FIRNFLOW_S3_ACCESS_KEY", "minioadmin");
    let secret = env_or("FIRNFLOW_S3_SECRET_KEY", "minioadmin");

    let credentials = Credentials::new(access, secret, None, None, "firnflow-test");
    let config = Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .endpoint_url(endpoint)
        .credentials_provider(credentials)
        .force_path_style(true)
        .build();
    Client::from_conf(config)
}

/// Real-AWS client: default credential chain (respects `AWS_PROFILE`),
/// region from `AWS_REGION` or falling back to eu-west-1.
async fn aws_client() -> Client {
    let region = env_or("AWS_REGION", "eu-west-1");
    let shared = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(region))
        .load()
        .await;
    Client::new(&shared)
}

/// Generic S3-compatible client for providers exposing an explicit
/// endpoint + static keys. Virtual-hosted style (path-style off):
/// R2, Tigris, and B2's S3 compat layers all accept it.
fn compat_client(endpoint: String, region: &str, access: String, secret: String) -> Client {
    let credentials = Credentials::new(access, secret, None, None, "firnflow-test");
    let config = Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(region.to_string()))
        .endpoint_url(endpoint)
        .credentials_provider(credentials)
        .force_path_style(false)
        .build();
    Client::from_conf(config)
}

/// Cloudflare R2. Region is `auto`, virtual-hosted style.
async fn r2_client() -> Option<Client> {
    let endpoint = std::env::var("R2_ENDPOINT").ok()?;
    let access = std::env::var("R2_ACCESS_KEY").ok()?;
    let secret = std::env::var("R2_SECRET_KEY").ok()?;
    Some(compat_client(endpoint, "auto", access, secret))
}

/// Tigris. Region is `auto`, virtual-hosted style.
async fn tigris_client() -> Option<Client> {
    let endpoint = std::env::var("TIGRIS_ENDPOINT").ok()?;
    let access = std::env::var("TIGRIS_ACCESS_KEY").ok()?;
    let secret = std::env::var("TIGRIS_SECRET_KEY").ok()?;
    Some(compat_client(endpoint, "auto", access, secret))
}

/// Backblaze B2. Region is encoded in the endpoint (e.g.
/// `s3.eu-central-003.backblazeb2.com` maps to `eu-central-003`).
async fn b2_client() -> Option<Client> {
    let endpoint = std::env::var("B2_ENDPOINT").ok()?;
    let access = std::env::var("B2_ACCESS_KEY").ok()?;
    let secret = std::env::var("B2_SECRET_KEY").ok()?;
    let region = std::env::var("B2_REGION").unwrap_or_else(|_| "us-west-004".into());
    Some(compat_client(endpoint, &region, access, secret))
}

/// DigitalOcean Spaces. Regional endpoint
/// (e.g. `https://lon1.digitaloceanspaces.com`); SigV4 signing region
/// defaults to `us-east-1` per DO's S3-compat docs but can be overridden
/// with `SPACES_REGION` if the regional code (`lon1`, `nyc3`, ...) is
/// required by a future Spaces release.
async fn spaces_client() -> Option<Client> {
    let endpoint = std::env::var("SPACES_ENDPOINT").ok()?;
    let access = std::env::var("SPACES_ACCESS_KEY").ok()?;
    let secret = std::env::var("SPACES_SECRET_KEY").ok()?;
    let region = std::env::var("SPACES_REGION").unwrap_or_else(|_| "us-east-1".into());
    Some(compat_client(endpoint, &region, access, secret))
}

/// Google Cloud Storage via the XML / Interoperability API. Region
/// default picks one half of a typical dual-region; any valid GCS
/// region name works for signing, and `auto` is also accepted.
async fn gcs_client() -> Option<Client> {
    let endpoint = std::env::var("GCS_ENDPOINT").ok()?;
    let access = std::env::var("GCS_ACCESS_KEY").ok()?;
    let secret = std::env::var("GCS_SECRET_KEY").ok()?;
    let region = std::env::var("GCS_REGION").unwrap_or_else(|_| "auto".into());
    Some(compat_client(endpoint, &region, access, secret))
}

async fn ensure_bucket(client: &Client, bucket: &str) {
    // CreateBucket is effectively idempotent for our purposes: any
    // real failure (credentials, network, region, ownership) will
    // surface loudly on the first PutObject below.
    let _ = client.create_bucket().bucket(bucket).send().await;
}

/// The shared assertion: two PUTs with `If-None-Match: *` to the same
/// key. First must succeed, second must fail with HTTP 412.
async fn assert_if_none_match_rejects_second_put(client: &Client, bucket: &str) {
    let key = unique_key("preflight/cond-write");

    client
        .put_object()
        .bucket(bucket)
        .key(&key)
        .body(ByteStream::from_static(b"first"))
        .if_none_match("*")
        .send()
        .await
        .expect("first PUT with If-None-Match=* should succeed on an empty key");

    let err = client
        .put_object()
        .bucket(bucket)
        .key(&key)
        .body(ByteStream::from_static(b"second"))
        .if_none_match("*")
        .send()
        .await
        .expect_err("second PUT with If-None-Match=* must fail because the key already exists");

    let status = match &err {
        SdkError::ServiceError(e) => e.raw().status().as_u16(),
        other => panic!("expected a ServiceError with HTTP status, got: {other:?}"),
    };
    assert_eq!(
        status, 412,
        "backend must return 412 Precondition Failed on the second If-None-Match=* PUT; \
         got {status}. A backend that silently ignores the precondition will pass at low \
         contention and fail Lance's CAS-based WAL under load, so do not proceed to the concurrent-writer stress."
    );

    // Best-effort cleanup; leaked keys are harmless for a throwaway bucket.
    let _ = client.delete_object().bucket(bucket).key(&key).send().await;
}

#[tokio::test]
#[ignore]
async fn put_object_with_if_none_match_rejects_second_write_minio() {
    let client = minio_client().await;
    let bucket = env_or("FIRNFLOW_S3_BUCKET", "firnflow-test");
    ensure_bucket(&client, &bucket).await;
    assert_if_none_match_rejects_second_put(&client, &bucket).await;
}

#[tokio::test]
#[ignore]
async fn put_object_with_if_none_match_rejects_second_write_aws() {
    if std::env::var("AWS_PROFILE").is_err() {
        eprintln!("SKIP: AWS_PROFILE not set; real-AWS pre-flight needs a configured CLI profile");
        return;
    }
    let client = aws_client().await;
    let bucket = env_or("FIRNFLOW_AWS_BUCKET", "firnflow-cloudfloe");
    // NOTE: we do not call `ensure_bucket` here. On real AWS the
    // bucket is pre-provisioned with public access blocked and should
    // remain reusable across runs; a CreateBucket attempt in the wrong
    // region (or against a name that's globally taken) gives worse
    // errors than a straightforward PutObject failure.
    assert_if_none_match_rejects_second_put(&client, &bucket).await;
}

#[tokio::test]
#[ignore]
async fn put_object_with_if_none_match_rejects_second_write_r2() {
    let Some(client) = r2_client().await else {
        eprintln!("SKIP: R2_ENDPOINT/R2_ACCESS_KEY/R2_SECRET_KEY not set");
        return;
    };
    let Ok(bucket) = std::env::var("R2_BUCKET") else {
        eprintln!("SKIP: R2_BUCKET not set");
        return;
    };
    assert_if_none_match_rejects_second_put(&client, &bucket).await;
}

#[tokio::test]
#[ignore]
async fn put_object_with_if_none_match_rejects_second_write_tigris() {
    let Some(client) = tigris_client().await else {
        eprintln!("SKIP: TIGRIS_ENDPOINT/TIGRIS_ACCESS_KEY/TIGRIS_SECRET_KEY not set");
        return;
    };
    let Ok(bucket) = std::env::var("TIGRIS_BUCKET") else {
        eprintln!("SKIP: TIGRIS_BUCKET not set");
        return;
    };
    assert_if_none_match_rejects_second_put(&client, &bucket).await;
}

#[tokio::test]
#[ignore]
async fn put_object_with_if_none_match_rejects_second_write_b2() {
    let Some(client) = b2_client().await else {
        eprintln!("SKIP: B2_ENDPOINT/B2_ACCESS_KEY/B2_SECRET_KEY not set");
        return;
    };
    let Ok(bucket) = std::env::var("B2_BUCKET") else {
        eprintln!("SKIP: B2_BUCKET not set");
        return;
    };
    assert_if_none_match_rejects_second_put(&client, &bucket).await;
}

#[tokio::test]
#[ignore]
async fn put_object_with_if_none_match_rejects_second_write_spaces() {
    let Some(client) = spaces_client().await else {
        eprintln!("SKIP: SPACES_ENDPOINT/SPACES_ACCESS_KEY/SPACES_SECRET_KEY not set");
        return;
    };
    let Ok(bucket) = std::env::var("SPACES_BUCKET") else {
        eprintln!("SKIP: SPACES_BUCKET not set");
        return;
    };
    assert_if_none_match_rejects_second_put(&client, &bucket).await;
}

#[tokio::test]
#[ignore]
async fn put_object_with_if_none_match_rejects_second_write_gcs() {
    let Some(client) = gcs_client().await else {
        eprintln!("SKIP: GCS_ENDPOINT/GCS_ACCESS_KEY/GCS_SECRET_KEY not set");
        return;
    };
    let Ok(bucket) = std::env::var("GCS_BUCKET") else {
        eprintln!("SKIP: GCS_BUCKET not set");
        return;
    };
    assert_if_none_match_rejects_second_put(&client, &bucket).await;
}

// -----------------------------------------------------------------------------
// GCS native conditional-write verification.
//
// `If-None-Match: *` is silently ignored on storage.googleapis.com; see
// the existing GCS row in the README compatibility matrix. The native
// path uses `object_store::gcp::GoogleCloudStorage` with
// `PutMode::Create`, which sets `x-goog-if-generation-match: 0` on the
// GCS XML PUT API ("fail if any version of this object exists"). This
// is the same code path that future native-GCS Firn support will rely
// on, so the test exercises the production mechanism rather than a
// third-party HTTP shape.
//
// An earlier iteration of these tests injected
// `x-goog-if-generation-match: 0` via the AWS SDK's
// `.customize().mutate_request()` hook. That approach is fundamentally
// incompatible with the AWS interop layer: the SDK auto-adds
// `x-amz-content-sha256` / `x-amz-date` during SigV4 signing, GCS
// rejects any request that mixes `x-amz-*` and `x-goog-*` header
// families with HTTP 400 `ExcessHeaderValues`, and the rejection
// happens before the precondition is even evaluated. The native
// `object_store::gcp` client hits the same XML PUT endpoint but
// authenticates with Google-native OAuth2 bearer tokens instead of
// SigV4 HMAC, so no `x-amz-*` headers are sent and the header-family
// clash never occurs.
// -----------------------------------------------------------------------------

use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutMode, PutOptions, PutPayload};

/// Build a GCS object store from environment for the native-CAS tests.
/// Returns `None` when the test should SKIP (missing config), and
/// panics if config is present but the build itself fails — a partially
/// configured environment is a misconfiguration, not a reason to
/// pretend the test passed.
fn gcs_native_store() -> Option<Arc<dyn ObjectStore>> {
    let bucket = std::env::var("GCS_BUCKET").ok()?;
    // Require *some* form of service-account credentials before we
    // bother building the client, so a missing config block surfaces
    // as a SKIP rather than a downstream auth error.
    if std::env::var("GOOGLE_APPLICATION_CREDENTIALS").is_err()
        && std::env::var("GOOGLE_SERVICE_ACCOUNT_PATH").is_err()
        && std::env::var("GOOGLE_SERVICE_ACCOUNT_KEY").is_err()
    {
        return None;
    }
    let store = GoogleCloudStorageBuilder::from_env()
        .with_bucket_name(&bucket)
        .build()
        .expect("GoogleCloudStorageBuilder::build() failed; check service-account JSON path and bucket name");
    Some(Arc::new(store))
}

#[tokio::test]
#[ignore]
async fn put_with_create_mode_rejects_second_write_gcs_native() {
    let Some(store) = gcs_native_store() else {
        eprintln!(
            "SKIP: set GCS_BUCKET and GOOGLE_APPLICATION_CREDENTIALS \
             (or GOOGLE_SERVICE_ACCOUNT_PATH / GOOGLE_SERVICE_ACCOUNT_KEY) \
             to run the GCS native-CAS pre-flight"
        );
        return;
    };

    let key = unique_key("preflight/native-cas");
    let path = ObjectPath::from(key.as_str());

    store
        .put_opts(
            &path,
            PutPayload::from_static(b"first"),
            PutOptions {
                mode: PutMode::Create,
                ..Default::default()
            },
        )
        .await
        .expect("first PutMode::Create on an empty key must succeed");

    let err = store
        .put_opts(
            &path,
            PutPayload::from_static(b"second"),
            PutOptions {
                mode: PutMode::Create,
                ..Default::default()
            },
        )
        .await
        .expect_err("second PutMode::Create must fail because the object now exists");

    assert!(
        matches!(err, object_store::Error::AlreadyExists { .. }),
        "second PutMode::Create on an existing key must surface as \
         object_store::Error::AlreadyExists; got {err:?}. GCS must reject the \
         second write to confirm the native generation precondition fires."
    );

    let _ = store.delete(&path).await;
}

/// Contended-key CAS microstress. Distinct-key stress does not test
/// CAS — every writer succeeds because there is no contention. The
/// only useful shape is N writers racing the SAME key. Exactly one
/// must observe a successful PUT; every other writer must observe an
/// `AlreadyExists` error.
///
/// A `tokio::sync::Barrier` is the load-bearing detail. Without it,
/// the first task spawned can finish its PUT before later tasks are
/// in flight, which weakens "contended" to "rapid sequential" — a
/// shape the precondition would also satisfy without serving any
/// linearisability evidence. Every task is held at the barrier
/// before request building begins; once all WRITERS tasks have
/// reached the gate they proceed together to construct and send
/// their PUTs.
#[tokio::test]
#[ignore]
async fn contended_key_create_mode_serialises_writers_gcs_native() {
    use tokio::sync::Barrier;

    const WRITERS: usize = 8;
    const ITERATIONS: usize = 100;

    let Some(store) = gcs_native_store() else {
        eprintln!(
            "SKIP: set GCS_BUCKET and GOOGLE_APPLICATION_CREDENTIALS \
             (or GOOGLE_SERVICE_ACCOUNT_PATH / GOOGLE_SERVICE_ACCOUNT_KEY) \
             to run the GCS native-CAS contended-key stress"
        );
        return;
    };

    for iter in 1..=ITERATIONS {
        let key = unique_key(&format!("preflight/native-cas-race-{iter:03}"));
        let path = ObjectPath::from(key.as_str());
        let barrier = Arc::new(Barrier::new(WRITERS));
        let mut handles = Vec::with_capacity(WRITERS);

        for w in 0..WRITERS {
            let store = Arc::clone(&store);
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            // Distinct payloads make divergence visible if the
            // assertion ever fails: a quick GET after the test tells
            // you which writer's body landed.
            let body: &'static [u8] = match w {
                0 => b"writer-0",
                1 => b"writer-1",
                2 => b"writer-2",
                3 => b"writer-3",
                4 => b"writer-4",
                5 => b"writer-5",
                6 => b"writer-6",
                _ => b"writer-7",
            };
            handles.push(tokio::spawn(async move {
                // Hold every writer at the gate until all WRITERS
                // tasks have reached this point. Building the request
                // and the put_opts call both happen after release, so
                // all tasks race the same starting line.
                barrier.wait().await;
                store
                    .put_opts(
                        &path,
                        PutPayload::from_static(body),
                        PutOptions {
                            mode: PutMode::Create,
                            ..Default::default()
                        },
                    )
                    .await
            }));
        }

        let mut wins = 0usize;
        let mut already_exists = 0usize;
        let mut other_failures = Vec::new();
        for h in handles {
            match h.await.expect("writer task panicked") {
                Ok(_) => wins += 1,
                Err(object_store::Error::AlreadyExists { .. }) => already_exists += 1,
                Err(other) => other_failures.push(format!("{other:?}")),
            }
        }

        assert!(
            other_failures.is_empty(),
            "iteration {iter}: contended-key race produced unexpected error types: \
             {other_failures:?}"
        );
        assert_eq!(
            wins, 1,
            "iteration {iter}: exactly one writer must win the contended race on \
             key {key}; got {wins} wins and {already_exists} AlreadyExists out of \
             {WRITERS} writers"
        );
        assert_eq!(
            already_exists,
            WRITERS - 1,
            "iteration {iter}: all losing writers must observe AlreadyExists on \
             key {key}; got {wins} wins and {already_exists} AlreadyExists out of \
             {WRITERS} writers"
        );

        let _ = store.delete(&path).await;
        if iter % 10 == 0 {
            eprintln!("gcs native-cas contended-key race {iter}/{ITERATIONS} clean");
        }
    }
}
