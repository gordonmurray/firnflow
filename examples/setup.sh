#!/usr/bin/env bash
# Create a virtualenv with the local firn wheel plus the dependencies the
# examples use (OpenCLIP on CPU torch, scikit-image, boto3).
set -Eeuo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WHEELS="${HERE}/../target/wheels"
cd "${HERE}"

if [ ! -d "${WHEELS}" ] || ! ls "${WHEELS}"/firn-*.whl >/dev/null 2>&1; then
    echo "No firn wheel in ${WHEELS}. Build it first:" >&2
    echo "  (cd .. && ./scripts/maturin build -m python/Cargo.toml -o target/wheels)" >&2
    exit 1
fi

python3 -m venv .venv
# shellcheck disable=SC1091
source .venv/bin/activate
pip install --upgrade pip
# CPU-only torch + torchvision, from the same index so their builds
# match (a mismatched torchvision fails with "torchvision::nms does not
# exist"). No CUDA, so the download stays a few hundred MB.
pip install torch torchvision --index-url https://download.pytorch.org/whl/cpu
pip install open_clip_torch scikit-image pillow boto3
pip install --force-reinstall --find-links "${WHEELS}" firn
echo "setup complete — run an example:  ./run.sh quickstart   |   ./run.sh clip"
