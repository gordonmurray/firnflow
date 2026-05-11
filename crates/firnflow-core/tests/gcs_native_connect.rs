//! Native GCS smoke test for `lancedb::connect("gs://...")`.
//!
//! Confirms that once the `gcs` feature is enabled on lancedb (and
//! the `gcp` feature on `object_store`), a connection to a real
//! Google Cloud Storage bucket resolves through `lance-io`'s
//! native GoogleCloudStorage backend rather than failing with
//! "No object store provider found for scheme: 'gs'". Listing
//! tables on a fresh prefix should succeed and return an empty
//! list — the assertion is "the round-trip completes" rather than
//! "we found any data".
//!
//! `#[ignore]`'d by default because it needs:
//! - `GCS_BUCKET` pointing at a writable bucket (no objects are
//!   created here; only `list_tables`).
//! - `GOOGLE_APPLICATION_CREDENTIALS` (or
//!   `GOOGLE_SERVICE_ACCOUNT_PATH` / `GOOGLE_SERVICE_ACCOUNT_KEY`)
//!   pointing at a service-account JSON with read access.
//!
//! Run with:
//!
//! ```text
//! GCS_BUCKET=firn-gcs-bucket-europe-west1 \
//! GOOGLE_APPLICATION_CREDENTIALS=/path/to/key.json \
//!     ./scripts/cargo test -p firnflow-core --test gcs_native_connect \
//!     -- --ignored --nocapture
//! ```

#[tokio::test]
#[ignore]
async fn lancedb_connect_resolves_gs_uri() {
    let Ok(bucket) = std::env::var("GCS_BUCKET") else {
        eprintln!("SKIP: GCS_BUCKET not set");
        return;
    };
    if std::env::var("GOOGLE_APPLICATION_CREDENTIALS").is_err()
        && std::env::var("GOOGLE_SERVICE_ACCOUNT_PATH").is_err()
        && std::env::var("GOOGLE_SERVICE_ACCOUNT_KEY").is_err()
    {
        eprintln!(
            "SKIP: set GOOGLE_APPLICATION_CREDENTIALS \
             (or GOOGLE_SERVICE_ACCOUNT_PATH / GOOGLE_SERVICE_ACCOUNT_KEY) \
             to run the GCS connect probe"
        );
        return;
    }

    // Use a per-run-unique prefix so repeated runs don't see each
    // other's state. The probe never writes; the prefix only
    // appears in the connection URI lance-io builds internally.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let uri = format!("gs://{bucket}/firnflow-connect-probe-{nanos}");
    eprintln!("connecting to {uri}");

    let conn = lancedb::connect(&uri)
        .execute()
        .await
        .expect("lancedb::connect should resolve a gs:// URI once the gcs feature is enabled");

    let tables = conn
        .table_names()
        .execute()
        .await
        .expect("listing tables under a fresh prefix should succeed and return an empty list");

    assert!(
        tables.is_empty(),
        "fresh probe prefix should have no tables; got {tables:?}"
    );
    eprintln!("connection ok, {} tables (expected 0)", tables.len());
}
