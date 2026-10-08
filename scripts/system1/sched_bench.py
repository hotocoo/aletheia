#!/usr/bin/env python3
"""Benchmark any System-1 sidecar on a scheduler corpus, over the decision wire (ADR-186).

    python3 scripts/system1/sched_bench.py --endpoint http://127.0.0.1:8091 CORPUS.jsonl \\
        [--threshold 0.9] [--limit N] [--out report.json]

The corpus is `kernel-core/examples/sched_corpus.rs` output, generated with a seed the model was
not trained on. Nothing here names a backend: every row goes to POST /v1/decide exactly as the
kernel's console would send a question, and the answer is compared with the label the real
`PriorityScheduler` produced. Reports accuracy per kind (what decided the row), and accuracy,
coverage and wrong-and-sure count at the escalation threshold.
"""
import argparse
import json
import sys
import urllib.request


def decide(endpoint, state, question):
    body = json.dumps({"state": state, "questions": [question]}).encode()
    req = urllib.request.Request(endpoint.rstrip("/") + "/v1/decide", data=body,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.load(r)["answers"][0]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("corpus")
    ap.add_argument("--endpoint", required=True)
    ap.add_argument("--threshold", type=float, default=0.9)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--out")
    a = ap.parse_args()

    with open(a.corpus) as f:
        rows = [json.loads(l) for l in f if l.strip()]
    if a.limit:
        rows = rows[: a.limit]
    per, sure, n_ok = {}, [], 0
    for i, r in enumerate(rows):
        ans = decide(a.endpoint, r["state"], r["question"])
        ok = ans["label"] == r["answer"]
        n_ok += ok
        k = per.setdefault(r["kind"], [0, 0])
        k[0] += ok
        k[1] += 1
        if ans["confidence"] >= a.threshold:
            sure.append(ok)
        if (i + 1) % 500 == 0:
            print("[sched_bench] %d/%d accuracy %.4f" % (i + 1, len(rows), n_ok / (i + 1)), file=sys.stderr, flush=True)
    report = {
        "corpus": a.corpus,
        "rows": len(rows),
        "accuracy_all": round(n_ok / max(1, len(rows)), 4),
        "accuracy": {k: round(v[0] / v[1], 4) for k, v in sorted(per.items())},
        "threshold": a.threshold,
        "coverage_at_threshold": round(len(sure) / max(1, len(rows)), 4),
        "accuracy_at_threshold": round(sum(sure) / max(1, len(sure)), 4),
        "wrong_and_sure": len(sure) - sum(sure),
    }
    print(json.dumps(report, indent=2))
    if a.out:
        with open(a.out, "w") as f:
            json.dump(report, f, indent=2)


if __name__ == "__main__":
    main()
