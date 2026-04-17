//! Spike-2 pre-flight.
//!
//! Verifies that the S3 backend honours `If-None-Match: *` on
//! PutObject before we trust Lance's CAS-based WAL on top of it.
//! CLAUDE.md mandates this check: a backend that silently ignores the
//! precondition will pass Lance's row-count assertion at low contention
//! and fail in production.
//!
//! Both tests are `#[ignore]`'d because they talk to out-of-process
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
//! ```

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
/// region from `AWS_REGION` or falling back to eu-west-1 per CLAUDE.md.
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
    let key = unique_key("spike2/cond-write");

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
         contention and fail Lance's CAS-based WAL under load, so do not proceed to spike-2b."
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
