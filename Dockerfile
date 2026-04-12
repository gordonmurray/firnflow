# Multi-stage build for the firnflow-api binary.
#
# Stage 1 compiles the binary against `rust:1.94-bookworm` with the
# same protobuf-compiler + libprotobuf-dev layer `Dockerfile.dev`
# installs (lance-encoding / lance-file need both at build time).
#
# Stage 2 is a minimal `debian:bookworm-slim` with just `ca-certificates`
# installed so the binary can talk to S3 over TLS. The release binary
# is self-contained otherwise (statically links everything except
# glibc).
#
# Build with:  docker build -t firnflow-api .
# Or via compose: `docker compose up --build firnflow`

FROM rust:1.94-bookworm AS builder

WORKDIR /build

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        protobuf-compiler \
        libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy the whole workspace so cargo can see every member crate.
# A docker bind mount / .dockerignore keeps target/ and
# .cargo-cache/ out of the build context.
COPY . .

RUN cargo build --release -p firnflow-api

# --- runtime ---
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/firnflow-api /usr/local/bin/firnflow-api

# foyer NVMe tier needs a writable directory; default to
# /var/lib/firnflow inside the container and surface it via the
# default `FIRNFLOW_CACHE_NVME_PATH`.
RUN mkdir -p /var/lib/firnflow/cache
ENV FIRNFLOW_CACHE_NVME_PATH=/var/lib/firnflow/cache
ENV FIRNFLOW_BIND=0.0.0.0:3000
ENV RUST_LOG=info

EXPOSE 3000

ENTRYPOINT ["/usr/local/bin/firnflow-api"]
