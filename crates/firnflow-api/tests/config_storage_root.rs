//! Storage-root resolver precedence tests.
//!
//! The resolver in `firnflow_api::config` is pure — it takes
//! `Option<&str>` for each env var and returns either a
//! [`ResolvedStorageRoot`] or an `anyhow::Error`. These tests drive
//! it with explicit values so every precedence branch is covered
//! without touching the process-global environment.
//!
//! Five precedence branches:
//!
//! 1. URI only → parse the URI; no fallback log.
//! 2. Bucket only → legacy fallback; fallback log fires.
//! 3. Both set, agreeing → use the URI silently.
//! 4. Both set, disagreeing → hard-fail with both raw values.
//! 5. Neither set → hard-fail naming both env vars.
//!
//! Plus dedicated coverage for the normalised-comparison rule (so
//! `s3://foo` and a bare `foo` agree) and the `gs://` rejection that
//! holds until native-GCS routing is wired in a follow-up release.

use firnflow_api::config::{resolve_storage_root, ResolvedStorageRoot};
use firnflow_core::StorageRoot;

fn expect_ok(uri: Option<&str>, bucket: Option<&str>) -> ResolvedStorageRoot {
    resolve_storage_root(uri, bucket).unwrap_or_else(|e| {
        panic!("expected resolver to succeed for uri={uri:?} bucket={bucket:?}, got: {e:#}")
    })
}

fn expect_err(uri: Option<&str>, bucket: Option<&str>) -> String {
    let err = resolve_storage_root(uri, bucket)
        .err()
        .unwrap_or_else(|| panic!("expected resolver to fail for uri={uri:?} bucket={bucket:?}"));
    format!("{err:#}")
}

#[test]
fn uri_only_uses_parsed_uri() {
    let out = expect_ok(Some("s3://from-uri/firn"), None);
    assert_eq!(out.root, StorageRoot::parse("s3://from-uri/firn").unwrap());
    assert!(
        !out.fallback_logged,
        "URI-only must not trigger the legacy-fallback log"
    );
}

#[test]
fn bucket_only_uses_legacy_fallback() {
    let out = expect_ok(None, Some("from-bucket"));
    assert_eq!(out.root, StorageRoot::s3_bucket("from-bucket").unwrap());
    assert!(
        out.fallback_logged,
        "bucket-only must trigger the legacy-fallback log so the operator sees the preference hint pointing at FIRNFLOW_STORAGE_URI"
    );
}

#[test]
fn both_agree_uses_uri_silently() {
    // s3://my-bucket and FIRNFLOW_S3_BUCKET=my-bucket normalise to
    // the same parsed StorageRoot. The resolver must accept this
    // without an error AND without the legacy-fallback log, because
    // the operator did set the preferred var.
    let out = expect_ok(Some("s3://my-bucket"), Some("my-bucket"));
    assert_eq!(out.root, StorageRoot::parse("s3://my-bucket").unwrap());
    assert!(
        !out.fallback_logged,
        "agreeing values must not trigger the legacy-fallback log"
    );
}

#[test]
fn both_agree_canonicalises_trailing_slash() {
    // s3://my-bucket/ and a bare bucket name normalise to the same
    // root under the parser's trailing-slash canonicalisation.
    // Comparison is on parsed structs, not raw strings.
    let out = expect_ok(Some("s3://my-bucket/"), Some("my-bucket"));
    assert!(!out.fallback_logged);
}

#[test]
fn both_disagree_fails_with_both_values() {
    // The error must surface both raw inputs so the operator sees
    // which var is wrong without grep-bouncing between the log and
    // their deployment config.
    let msg = expect_err(Some("s3://one"), Some("two"));
    assert!(msg.contains("\"s3://one\""), "missing URI in error: {msg}");
    assert!(msg.contains("\"two\""), "missing bucket in error: {msg}");
    assert!(
        msg.contains("FIRNFLOW_STORAGE_URI") && msg.contains("FIRNFLOW_S3_BUCKET"),
        "error must name both env vars: {msg}"
    );
}

#[test]
fn neither_set_fails_with_actionable_message() {
    let msg = expect_err(None, None);
    assert!(
        msg.contains("FIRNFLOW_STORAGE_URI") && msg.contains("FIRNFLOW_S3_BUCKET"),
        "neither-set error must name both env vars so the operator knows the fix: {msg}"
    );
}

#[test]
fn empty_strings_are_treated_as_unset() {
    // env::var returns Ok("") when the var is exported empty. The
    // resolver normalises this to "unset" via a trim+filter so
    // FIRNFLOW_STORAGE_URI="" + FIRNFLOW_S3_BUCKET=foo behaves as
    // bucket-only, not as both-set.
    let out = expect_ok(Some(""), Some("only-bucket"));
    assert_eq!(out.root, StorageRoot::s3_bucket("only-bucket").unwrap());
    assert!(out.fallback_logged);

    let out = expect_ok(Some("s3://only-uri"), Some(""));
    assert_eq!(out.root, StorageRoot::parse("s3://only-uri").unwrap());
    assert!(!out.fallback_logged);

    let msg = expect_err(Some("  "), Some(""));
    assert!(msg.contains("FIRNFLOW_STORAGE_URI"), "msg: {msg}");
}

#[test]
fn gs_uri_is_rejected_as_unsupported() {
    // GCS remains disabled until native routing is wired: gs://
    // must surface as Unsupported rather than as a generic parse
    // failure. The wrapping anyhow context still names
    // FIRNFLOW_STORAGE_URI so the operator knows which env var to
    // fix; the inner message names the scheme and the forward-
    // looking note about the planned follow-up release.
    let msg = expect_err(Some("gs://firn-gcs-bucket"), None);
    assert!(
        msg.contains("FIRNFLOW_STORAGE_URI"),
        "error must name the offending env var: {msg}"
    );
    assert!(
        msg.contains("follow-up release") || msg.contains("not supported"),
        "error must mention that GCS is a planned but not-yet-routable scheme: {msg}"
    );
}

#[test]
fn malformed_uri_is_rejected() {
    let msg = expect_err(Some("not-a-uri"), None);
    assert!(msg.contains("FIRNFLOW_STORAGE_URI"), "msg: {msg}");
}

#[test]
fn empty_bucket_name_is_rejected() {
    // Whitespace-only trims to empty in the resolver's filter, so
    // this falls through to the neither-set branch. Either outcome
    // (a neither-set message OR a bucket-specific message) is
    // acceptable as long as the operator sees an env-var name they
    // can act on.
    let msg = expect_err(None, Some("   "));
    assert!(
        msg.contains("FIRNFLOW_STORAGE_URI") || msg.contains("FIRNFLOW_S3_BUCKET"),
        "error must name an env var: {msg}"
    );
}
