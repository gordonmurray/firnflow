# firn Python examples

Runnable examples for the `firn` Python package (built from `../python`).

## Setup

```bash
./setup.sh
```

Creates a virtualenv and installs `firn` (from the local wheel in
`../target/wheels`) plus the dependencies the examples use (OpenCLIP on
CPU torch, scikit-image, boto3).

## quickstart — local, no infrastructure

```bash
./run.sh quickstart
```

Writes to `./firn_data_demo`, then runs vector, full-text (BM25), and
hybrid search. No server, no credentials.

## clip — image search on object storage

```bash
export TIGRIS_ACCESS_KEY=...
export TIGRIS_SECRET_KEY=...
export TIGRIS_BUCKET=firn-tigris-bucket   # optional; this is the default
./run.sh clip
```

Embeds a few sample photos with OpenCLIP, stores the photo bytes as
Tigris objects and the embeddings in firn (also on Tigris), then
searches by text ("a cat") and by image. The first run downloads the
CLIP weights (~350 MB, then cached); sample photos are bundled with
scikit-image.
