"""Score the index at several refine_factor settings, with nprobes fixed.

For each refine_factor value, this sends the same vectors to
POST /ns/{ns}/query and counts how many returned ids appear in the exact
top-k. That fraction, averaged over all queries, is recall@k.

refine_factor=0 (or omitting it) measures the baseline: IVF_PQ only,
no post-index re-scoring. Any positive value fetches N * k candidates
from the index and then re-scores them against the full stored vectors,
keeping the true top-k from that re-scored set.

The same cache-hit rejection and response-integrity checks as
recall_sweep.py apply here. A non-zero cache_hits count stops the run
before the output is written. A response with anything other than k
distinct ids also stops the run.

Usage:
    python refine_factor_sweep.py NAMESPACE TRUTH.npz K NPROBES RF_LIST OUTPUT.json

RF_LIST is comma-separated refine_factor values to test. Use 0 for the
unrefined baseline. For example: "0,5,10,20".

Example:
    python refine_factor_sweep.py wiki100k truth.npz 10 20 0,5,10,20 rf_sweep.json
"""

import json
import os
import statistics
import sys
import time

import numpy as np
import requests

from corpus import BASE_URL, auth_headers, cache_hit_count, percentile, unit_normalise
from recall_sweep import check_ids, load_truth, refuse_existing_outputs, reject

WARMUP_OFFSET = np.float32(1e-3)


def run_query(session, namespace, vector, k, nprobes, refine_factor):
    body = {
        "vector": vector,
        "k": k,
        "nprobes": nprobes,
        "include_vector": False,
    }
    if refine_factor:
        body["refine_factor"] = refine_factor
    started = time.perf_counter()
    response = session.post(
        f"{BASE_URL}/ns/{namespace}/query",
        json=body,
        headers=auth_headers(),
        timeout=600,
    )
    elapsed_ms = (time.perf_counter() - started) * 1000.0
    response.raise_for_status()
    return [hit["id"] for hit in response.json()["results"]], elapsed_ms


def score_setting(session, namespace, queries, truth_ids, k, nprobes, refine_factor):
    warmup = unit_normalise(queries + WARMUP_OFFSET)
    for vector in warmup:
        run_query(session, namespace, vector.tolist(), k, nprobes, refine_factor)

    hits_before = cache_hit_count(namespace)
    overlaps, latencies = [], []
    for index, vector in enumerate(queries):
        ids, elapsed_ms = run_query(
            session, namespace, vector.tolist(), k, nprobes, refine_factor
        )
        reason = check_ids(ids, k)
        if reason is not None:
            return None, {
                "refine_factor": refine_factor,
                "query": index,
                "reason": reason,
            }
        truth = set(truth_ids[index].tolist())
        overlaps.append(len(truth & set(ids)) / len(truth))
        latencies.append(elapsed_ms)
    hits_after = cache_hit_count(namespace)

    ordered = sorted(latencies)
    row = {
        "refine_factor": refine_factor,
        "nprobes": nprobes,
        f"recall@{k}": round(float(np.mean(overlaps)), 4),
        "queries": len(queries),
        "p50_ms": round(statistics.median(latencies), 2),
        "p95_ms": round(percentile(ordered, 0.95), 2),
        "p99_ms": round(percentile(ordered, 0.99), 2),
        "cache_hits": hits_after - hits_before,
    }
    return row, None


def main():
    namespace = sys.argv[1]
    truth_path = sys.argv[2]
    k = int(sys.argv[3])
    nprobes = int(sys.argv[4])
    rf_list = [int(v) for v in sys.argv[5].split(",")]
    output = sys.argv[6]
    refuse_existing_outputs(output)

    queries, truth_ids = load_truth(truth_path, k)
    session = requests.Session()

    report = []
    for rf in rf_list:
        row, fault = score_setting(
            session, namespace, queries, truth_ids, k, nprobes, rf
        )
        if fault is not None:
            reject(
                output,
                report,
                [fault],
                f"at refine_factor {rf}, query {fault['query']} "
                f"{fault['reason']}.",
            )
        report.append(row)
        print(json.dumps(row), flush=True)

    stale = [row["refine_factor"] for row in report if row["cache_hits"]]
    if stale:
        reject(
            output,
            report,
            [],
            f"the result cache answered queries at refine_factor {stale}. "
            f"Clear the cache and run again.",
        )

    with open(output, "w") as handle:
        json.dump(report, handle, indent=2)
    print(f"wrote {output}")


if __name__ == "__main__":
    main()
