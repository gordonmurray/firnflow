#!/usr/bin/env bash
# Run one of the examples. Needs ./setup.sh first.
#
#   ./run.sh quickstart   # local, no credentials
#   ./run.sh clip          # CLIP image search on Tigris (needs creds)
set -Eeuo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${HERE}"
[ -d .venv ] || {
    echo "run ./setup.sh first" >&2
    exit 1
}
# shellcheck disable=SC1091
source .venv/bin/activate

case "${1:-quickstart}" in
quickstart)
    python quickstart.py
    ;;
clip)
    : "${TIGRIS_ACCESS_KEY:?set TIGRIS_ACCESS_KEY for the clip example}"
    : "${TIGRIS_SECRET_KEY:?set TIGRIS_SECRET_KEY for the clip example}"
    python clip_search.py
    ;;
*)
    echo "unknown example '${1}'. Try: quickstart | clip" >&2
    exit 1
    ;;
esac
