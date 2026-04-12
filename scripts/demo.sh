#!/usr/bin/env bash
# Firn demo script for asciinema recording.
#
# Usage:
#   1. Make sure MinIO and the API are running:
#        docker compose up -d minio minio-init
#        FIRNFLOW_S3_BUCKET=firnflow-test \
#        FIRNFLOW_S3_ENDPOINT=http://127.0.0.1:9000 \
#        FIRNFLOW_S3_ACCESS_KEY=minioadmin \
#        FIRNFLOW_S3_SECRET_KEY=minioadmin \
#          ./scripts/cargo run -p firnflow-api
#
#   2. In another terminal:
#        asciinema rec demo.cast
#        bash scripts/demo.sh
#        exit
#
#   3. Upload:
#        asciinema upload demo.cast

set -euo pipefail

# Simulates typing then runs the command. The viewer sees each
# character appear with a small delay, making it look hand-typed.
type_and_run() {
    local cmd="$1"
    printf '\n\033[1;32m$\033[0m '
    echo "$cmd" | while IFS= read -r -n1 char; do
        printf '%s' "$char"
        sleep 0.03
    done
    echo ""
    sleep 0.3
    eval "$cmd"
    sleep 1.5
}

clear
echo ""
echo "  ┌─────────────────────────────────────────┐"
echo "  │  Firn — vector + FTS search on S3       │"
echo "  │  github.com/gordonmurray/firnflow       │"
echo "  └─────────────────────────────────────────┘"
echo ""
sleep 2

# 1. Health check
type_and_run 'curl -s http://localhost:3000/health && echo ""'

# 2. Upsert rows with vectors and text
type_and_run 'curl -s -X POST http://localhost:3000/ns/demo/upsert \
  -H "content-type: application/json" \
  -d '"'"'{"rows":[
    {"id":1,"vector":[1,0,0,0,0,0,0,0],"text":"the quick brown fox jumps"},
    {"id":2,"vector":[0,1,0,0,0,0,0,0],"text":"a lazy dog sleeps quietly"},
    {"id":3,"vector":[0,0,1,0,0,0,0,0],"text":"red car drives through rain"}
  ]}'"'"' | jq .'

# 3. Cold query — first time, hits S3
echo ""
echo "  --- cold query (first time — hits S3) ---"
sleep 1
type_and_run 'time curl -s -X POST http://localhost:3000/ns/demo/query \
  -H "content-type: application/json" \
  -d '"'"'{"vector":[1,0,0,0,0,0,0,0],"k":2}'"'"' | jq .'

# 4. Warm query — same query, served from cache
echo ""
echo "  --- warm query (same query — served from cache) ---"
sleep 1
type_and_run 'time curl -s -X POST http://localhost:3000/ns/demo/query \
  -H "content-type: application/json" \
  -d '"'"'{"vector":[1,0,0,0,0,0,0,0],"k":2}'"'"' | jq .'

# 5. Build FTS index (required before text queries)
echo ""
echo "  --- building full-text search index ---"
sleep 1
type_and_run 'curl -s -X POST http://localhost:3000/ns/demo/fts-index | jq .'
sleep 2

# 6. FTS query — search by text
echo ""
echo "  --- full-text search ---"
sleep 1
type_and_run 'curl -s -X POST http://localhost:3000/ns/demo/query \
  -H "content-type: application/json" \
  -d '"'"'{"text":"lazy","k":2}'"'"' | jq .'

# 7. Show the metrics proving cache saved the S3 trip
echo ""
echo "  --- proof: cache hit saved an S3 round-trip ---"
sleep 1
type_and_run 'curl -s http://localhost:3000/metrics | grep -E "cache_hits|cache_misses|s3_requests_total"'

# 8. Clean up
echo ""
echo "  --- delete namespace ---"
sleep 1
type_and_run 'curl -s -X DELETE http://localhost:3000/ns/demo | jq .'

echo ""
echo ""
echo "  Cache miss = 1 S3 query. Cache hit = 0."
echo "  That is the whole point."
echo ""
sleep 3
