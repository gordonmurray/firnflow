# Provider support matrix

Captures which S3-compatible backends have been validated for the
hard-problem spikes in `CLAUDE.md` § "Known hard problems". A backend
listed here as **validated** has passed both the `If-None-Match: *`
pre-flight and the LanceDB concurrent-writer stress test; anything
else is **unvalidated** and must not be trusted in production until
it reaches validated status.

## Test runner

| Component        | Version                                     |
| ---------------- | ------------------------------------------- |
| Rust toolchain   | 1.94.1 (via `rust:1.94-bookworm`)           |
| `aws-sdk-s3`     | 1.x (`^1`, resolved via `Cargo.lock`)       |
| `aws-config`     | 1.x (`^1`, resolved via `Cargo.lock`)       |
| `lancedb`        | 0.27.2 (exact-pinned, `features = ["aws"]`) |
| `lance`          | 4.0.0 (exact-pinned)                        |
| `arrow-array`    | 57.x                                        |

**Note on the `aws` feature**: lancedb 0.27.2 gates its S3 object store
provider behind the `aws` cargo feature. Without it, `s3://` URIs fail
at runtime with *"No object store provider found for scheme: 's3'"*.
This is easy to miss because compilation succeeds without the feature.

## MinIO (local dev)

- **Image**: `minio/minio:RELEASE.2025-09-07T16-13-09Z`
- **Digest**: `sha256:14cea493d9a34af32f524e538b8346cf79f3321eff8e708c1e2960462bd8936e`
- **First validated**: 2026-04-11

### Spike-2a — `If-None-Match: *` pre-flight

- **Status**: PASSED (2026-04-11)
- **Test**: `crates/firnflow-core/tests/s3_conditional_writes.rs::put_object_with_if_none_match_rejects_second_write_minio`
- **Observed**: first PUT with `If-None-Match: *` returned 200;
  second PUT to the same key with `If-None-Match: *` returned
  HTTP 412 as required.

### Spike-2b — LanceDB concurrent-writer stress test

- **Status**: PASSED (2026-04-11), 100 runs, 0 discrepancies
- **Test**: `crates/firnflow-core/tests/lance_concurrent_writes.rs::concurrent_writers_100_runs_minio`
- **Parameters**: 8 concurrent writers × 100 rows each = 800 expected
  rows per run, 100 runs
- **Observed**: every run returned exactly 800 rows; total wall time
  92 s (~920 ms per iteration including fresh namespace + 8 CAS
  writes + count_rows).

## AWS S3

- **Credentials**: named CLI profile `cloudfloe` (account
  `016230046494`, `terraform-user`).
- **Region**: `eu-west-1` (no region configured on the profile; set
  via `AWS_REGION`, matches the CLAUDE.md default).
- **Test bucket**: `lakestream-spike2-cloudfloe` (created 2026-04-11,
  public access blocked per the global CLAUDE.md S3 defaults).
- **First validated**: 2026-04-11

### Spike-2a — `If-None-Match: *` pre-flight

- **Status**: PASSED (2026-04-11)
- **Test**: `crates/firnflow-core/tests/s3_conditional_writes.rs::put_object_with_if_none_match_rejects_second_write_aws`
- **Observed**: first PUT with `If-None-Match: *` returned 200;
  second PUT to the same key with `If-None-Match: *` returned
  HTTP 412 as required.
- **Run command**: `AWS_PROFILE=cloudfloe ./scripts/cargo test -p firnflow-core --test s3_conditional_writes -- --ignored`

### Spike-2b — LanceDB concurrent-writer stress test

- **Status**: PASSED (2026-04-11), 100 runs, 0 discrepancies
- **Test**: `crates/firnflow-core/tests/lance_concurrent_writes.rs::concurrent_writers_100_runs_aws`
- **Parameters**: 8 concurrent writers × 100 rows each = 800 expected
  rows per run, 100 runs
- **Observed**: every run returned exactly 800 rows; total wall time
  337 s (~3.37 s per iteration including fresh namespace + 8 CAS
  writes + count_rows, ~3.7× MinIO owing to real S3 round-trip latency).

## GCS / R2

- **Status**: UNVALIDATED. Out of scope for the current session.
- **Next action**: run the same spike-2a/2b tests once the operator
  supplies credentials and confirms these as realistic deployment
  targets (CLAUDE.md § 2 explicitly treats these as conditional).

## Known housekeeping

The `lakestream-spike2-cloudfloe` bucket accumulates one namespace
prefix per stress iteration (100 per 100-run invocation). Each
namespace holds an empty seed batch + 8 small append batches,
~a few KB per run. There is no automatic cleanup yet; prune with
`aws s3 rm --recursive s3://lakestream-spike2-cloudfloe/spike2b-*`
under `AWS_PROFILE=cloudfloe` when the bucket's object count gets
annoying. Consider adding a `spike2-cleanup` `#[ignore]`'d test or
shell script if this becomes routine.
