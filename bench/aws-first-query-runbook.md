# First-query profile on real S3: operator runbook

This is the procedure for running the `first_query_profile` benchmark
against real AWS S3 from an EC2 instance, so the cold first-query
numbers reflect genuine object-storage latency rather than a local
MinIO loopback. The harness itself, its measurement cases, and its
environment variables are documented in the rustdoc header of
`crates/firnflow-bench/src/bin/first_query_profile.rs`. This file
covers the parts that live outside the binary: instance choice,
credentials, bucket-side request attribution, the two runs (smoke then
headline), and how to read the result.

Set two shell variables to your own values and the commands below run
as written:

```bash
BUCKET=your-bench-bucket   # an S3 bucket you control
REGION=eu-west-1           # the region that bucket lives in
```

## What this measures, and why it needs real S3

The result cache only helps a query that is an exact repeat of an
earlier one. A novel or cold query pays the full cost of reading
Lance's index and data files over the network. The cold-vs-warm
numbers committed under `bench/results/` were measured against MinIO on
loopback, where that network cost is close to zero, so they understate
the real first-query latency. This run answers the question the
loopback benchmark cannot: on real S3, how long does the first query
against a freshly opened namespace actually take, and where does the
time go.

The five measurement cases (`cold-process`, `warm-identical`,
`warm-novel`, `dropped-handle`, `fresh-process`) isolate the
contributions of process startup, the LanceDB handle pool, and the AWS
SDK connection warmup. All five bypass the foyer result cache by
calling `NamespaceManager::query` directly, so none of them is a cache
hit.

## Run it on a dedicated instance, not on production

The instance must sit in the same region as the bucket. A cross-region
instance would add inter-region round-trip latency to every object
read and make the numbers meaningless.

Use a short-lived dedicated instance, not a box that serves live
traffic. The headline run seeds a million 1536-dimension vectors and
builds an IVF_PQ index, which is a memory and CPU surge. On a 16 GiB
instance with no swap that also serves a live site, an index build
competing for memory can trigger the OOM killer and take down a live
container. A dedicated instance costs a couple of dollars for the hour
or two the run takes, isolates the numbers from production traffic, and
is torn down afterwards.

## Instance and IAM

- **Type**: `m7i.2xlarge` (8 vCPU, 32 GiB). The headroom keeps the
  index build comfortable and the build of the harness itself fast.
  `m7i.xlarge` works if cost matters more than wall-clock.
- **AMI**: latest Amazon Linux 2023, in `$REGION`.
- **Root volume**: gp3, 80 GiB. The million-row Lance dataset plus the
  index is a few GiB; the harness build tree and the foyer NVMe cache
  directory take the rest.
- **Networking**: a public subnet (or a private subnet with SSM
  reachability), no inbound rules. Access is over SSM, so the instance
  needs no public SSH.
- **IAM instance profile** with two grants:
  - `AmazonSSMManagedInstanceCore` (managed policy) for SSM access.
  - S3 access to the bucket: `s3:GetObject`, `s3:PutObject`,
    `s3:DeleteObject`, `s3:ListBucket` on `arn:aws:s3:::$BUCKET` and
    `arn:aws:s3:::$BUCKET/*`.

With the instance profile attached, the harness reads credentials from
the default chain (instance metadata), so `FIRNFLOW_S3_ACCESS_KEY` and
`FIRNFLOW_S3_SECRET_KEY` are left unset.

## Build the harness on the instance

Amazon Linux 2023 uses a newer glibc than the Debian-based dev
container, so a binary built on the dev host will not run here. Build
natively on the instance:

```bash
sudo dnf install -y git gcc gcc-c++ make protobuf-compiler protobuf-devel
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
  --default-toolchain 1.94.0
source "$HOME/.cargo/env"
git clone https://github.com/gordonmurray/firnflow.git
cd firnflow
cargo build --release -p firnflow-bench --bin first_query_profile
```

If `protobuf-compiler` and `protobuf-devel` are not in the instance's
repos, install protoc from a prebuilt release instead; the release zip
ships the well-known `.proto` files Lance's build imports. The build
pulls and compiles the full Lance and DataFusion graph, so the first
build takes a while; on an `m7i.2xlarge` it is comfortably under the
index-build time that follows.

## Bucket-side request attribution

The harness reports a `s3_requests delta` per case, but that counter
only tracks calls at firnflow's service boundary, not the raw range
GETs that `object_store` issues underneath. To see the real S3
request and byte counts, attribute them on the bucket side. The
harness pins all of its data under a single namespace prefix and
records the `start_utc` / `stop_utc` window in the report, so either
can be used as the filter.

The cheap option is S3 server access logging: enable it on the bucket
(logging to a second bucket) before the run, then filter the delivered
logs by the `/{namespace}/` key prefix. The lower-latency option is a
CloudWatch request metrics filter scoped to the same prefix, which
surfaces `GetRequests` and `BytesDownloaded` within minutes rather than
waiting for log delivery. Set whichever you prefer up before the
seeding step so the index build and the query cases are both captured.

## Run 1: smoke (250k rows)

Validate the whole path end to end at a smaller scale first. This
confirms the harness runs against real S3 under the current Lance pins,
the report comes out in the expected shape, and the credentials and
prefix attribution are wired correctly, all before committing to the
long run.

```bash
FIRNFLOW_STORAGE_URI=s3://$BUCKET \
FIRNFLOW_S3_REGION=$REGION \
FIRNFLOW_PROFILE_NAMESPACE=first-query-profile-smoke \
FIRNFLOW_PROFILE_ROWS=250000 \
FIRNFLOW_PROFILE_DIM=1536 \
FIRNFLOW_PROFILE_REPS=10 \
FIRNFLOW_PROFILE_OUT=bench/results/first_query_profile_smoke.md \
  ./target/release/first_query_profile
```

Read `bench/results/first_query_profile_smoke.md` and sanity-check it:
`cold-process` p50 should be clearly slower than `warm-identical`, and
the seed line should show a real upsert / index / compact time. The
`s3_requests delta` column reads 0 for every case, which is expected,
not a fault: the measurement cases call `NamespaceManager::query`
directly, and that counter only moves on the service-boundary
operations (upsert, index, compact), so the query cases never touch it.
Bucket-side attribution (above) is the real S3 signal. If the cold and
warm numbers come out identical, or the namespace reads back empty, fix
it here, not after the headline run.

## Run 2: headline (1M rows, 1536-dim)

```bash
FIRNFLOW_STORAGE_URI=s3://$BUCKET \
FIRNFLOW_S3_REGION=$REGION \
FIRNFLOW_PROFILE_NAMESPACE=first-query-profile-1m \
FIRNFLOW_PROFILE_ROWS=1000000 \
FIRNFLOW_PROFILE_DIM=1536 \
FIRNFLOW_PROFILE_REPS=20 \
FIRNFLOW_PROFILE_OUT=bench/results/first_query_profile.md \
  ./target/release/first_query_profile
```

`reps=20` gives the percentile columns enough samples to be worth
reading; at very low rep counts p95 and p99 collapse onto the max,
which the harness notes in the report. The seeding step (upsert one
million vectors, build the index, compact) dominates the wall-clock and
runs once before the measurement cases.

Pull the report back off the instance with SSM (or read it inline
through the SSM command output) and commit it to `bench/results/`.

## The lower-bound caveat

The `cold-process` case rebuilds the `NamespaceManager`, cache, and
service for each repetition, but it runs every repetition inside one
process, so the AWS SDK HTTP client pool, TLS sessions, and the Tokio
runtime stay warm across reps. The reported `cold-process` number is
therefore a lower bound on what a genuinely new OS process would see.
The `fresh-process` case runs the same shape after all the other cases
have warmed the SDK, so the gap between `cold-process` and
`fresh-process` is a coarse read on how much of the first-query cost is
SDK and connection setup rather than object reads.

A true fresh-process measurement needs an outer driver that launches a
new binary per repetition. If the in-process numbers leave the
SDK-warmup question open, wrap the binary in a shell loop that runs one
rep per invocation against the already-seeded namespace:

```bash
for i in $(seq 1 20); do
  FIRNFLOW_STORAGE_URI=s3://$BUCKET \
  FIRNFLOW_S3_REGION=$REGION \
  FIRNFLOW_PROFILE_NAMESPACE=first-query-profile-1m \
  FIRNFLOW_PROFILE_SEED=false \
  FIRNFLOW_PROFILE_REPS=1 \
  FIRNFLOW_PROFILE_OUT=bench/results/fresh_process_$i.md \
    ./target/release/first_query_profile
done
```

Each invocation pays the real process and SDK startup once, so the
first (and only) recorded query per run is a true cold-process sample.
Collect the per-run numbers by hand into a single distribution.

## Teardown

Terminate the instance once the report is committed. The seeded
namespaces (`first-query-profile-smoke`, `first-query-profile-1m`)
stay in the bucket; delete them through the API
(`DELETE /ns/{namespace}`) or leave them for a re-run with
`FIRNFLOW_PROFILE_SEED=false`. If S3 access logging was enabled only
for this run, turn it back off.

## Reading the result, and what to do next

The headline numbers tell you which problem the project actually has,
which in turn points at the next piece of work:

- **If the cold first query is slow and most of the time is object
  reads** (large `warm-novel` and `dropped-handle` numbers, confirmed
  by the bucket-side request and byte counts over the run window), the
  bottleneck is the index and data reads over the network. That is the
  case the local object cache (`FIRNFLOW_OBJECT_CACHE_ENABLED`) is built
  to address, and the next step is to re-run with the object cache on
  and measure the difference on the second cold query. A `dropped-handle`
  number close to `cold-process` (with `warm-novel` well below both)
  points specifically at the table-open reads, the manifest and index
  metadata pulled every time a namespace handle is opened, which the
  object cache also covers.
- **If most of the cold time is process and SDK startup** (a large gap
  between `cold-process` / `fresh-process` and the warm cases, but a
  small `warm-novel` number), the object-store path is not the
  bottleneck and caching index bytes will not move the headline. Say so
  and direct effort at the operational layer instead.
- **If the warm-novel number is already low on real S3**, Lance's own
  index plus the handle pool are doing most of the work, and the cache
  story should be framed around recurring queries only, not novel ones.

Whichever way it falls, the report is the second public benchmark (the
first being the cold-vs-warm result-cache numbers), and it is the
evidence base for the cold-path framing in the README.

## First run (2026-06-17)

The first execution against an AWS S3 bucket in `eu-west-1`, from an
`m7i.2xlarge`, landed in the first branch of the decision tree: the cold
path is object-read bound, dominated by table-open. Three reports are
committed alongside this runbook.

- `first_query_profile_smoke.md` (250k rows) and
  `first_query_profile.md` (1M rows), object cache off. Cold first query
  p50 was 667 ms at 250k and 719 ms at 1M, growing only 8 percent for a
  4x larger dataset because it is bound by fixed table-open cost, not
  data volume. `dropped-handle` tracked `cold-process` closely while
  `warm-novel` sat near 200 ms, which is the signature of table-open
  (manifest plus index metadata read on every handle open) being the
  dominant cost. The handle pool removes that cost on subsequent
  queries.
- `first_query_profile_objcache.md` (1M rows) re-ran the same namespace
  with the local object cache on (`FIRNFLOW_PROFILE_OBJECT_CACHE=true`).
  Once the cache was warm, a novel query dropped from 214 ms to 7 ms
  (about 30x), cold-process from 719 ms to 383 ms, and `dropped-handle`
  from 690 ms to 279 ms, serving Lance's index and metadata reads from
  local NVMe instead of S3. The very first query against the namespace
  (the 932 ms probe) is not accelerated; it populates the cache. Over
  the run the cache took 7,823 hits against 1,853 misses and fetched
  about 92 MB from S3, with no evictions inside the 10 GiB budget.

The object-cache numbers are measured with the cache already warm. The
framing for any public use is that the first read of a byte range still
pays the S3 cost; the object cache accelerates every read after that,
including novel queries the result cache cannot help.
