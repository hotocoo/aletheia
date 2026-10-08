#!/usr/bin/env python3
"""Answer parity between two System-1 runtimes serving the same checkpoint (ADR-241).

    python3 scripts/system1/native_parity.py CORPUS.jsonl REFERENCE_ENDPOINT CANDIDATE_ENDPOINT \\
        [--rows 400] [--threshold 0.9] [--seed 240] [--batch 3]

Every sampled corpus row goes to both endpoints over POST /v1/decide, one question per request;
then `--batch` rows at a time go as one request with an extra yes/no question, so the padded batch
path is compared too. Reports, per runtime, accuracy against the corpus label and p50/p95
latency; between them, label parity, agreement on the side of the escalation threshold, and the
largest confidence difference. Exit 1 when any answer differs. CI-exempt: it needs a checkpoint.
"""
import argparse
import json
import random
import sys
import time
import urllib.error
import urllib.request


def ask(ep, state, questions):
    body = json.dumps({"state": state, "questions": questions}).encode()
    t = time.perf_counter()
    try:
        with urllib.request.urlopen(urllib.request.Request(ep.rstrip("/") + "/v1/decide", data=body), timeout=120) as r:
            out = json.load(r)["answers"]
    except urllib.error.HTTPError as e:
        out = [{"error": e.read().decode()[:120]}] * len(questions)
    return out, (time.perf_counter() - t) * 1000


def key(a):
    """What an answer says: its label or yes/no, or the refusal's message (formatting aside)."""
    if "error" in a:
        try:
            return ("refused", json.loads(a["error"]).get("error"))
        except ValueError:
            return ("refused", a["error"])
    return (a.get("label"), a.get("yes"))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("corpus")
    ap.add_argument("reference")
    ap.add_argument("candidate")
    ap.add_argument("--rows", type=int, default=400)
    ap.add_argument("--threshold", type=float, default=0.9)
    ap.add_argument("--seed", type=int, default=240)
    ap.add_argument("--batch", type=int, default=3)
    a = ap.parse_args()

    rows = [json.loads(l) for l in open(a.corpus) if l.strip()]
    random.Random(a.seed).shuffle(rows)
    rows = rows[: a.rows]
    eps = [a.reference, a.candidate]
    single = {ep: [] for ep in eps}
    for r in rows:
        q = dict(r["question"], type=r["question"].get("type", "choice"))
        for ep in eps:
            single[ep].append(ask(ep, r["state"], [q]))
    gold = [r.get("answer") for r in rows]
    for ep in eps:
        lat = sorted(t for _, t in single[ep])
        acc = sum(1 for (ans, _), g in zip(single[ep], gold) if ans[0].get("label") == g)
        print("%-28s accuracy %d/%d  p50 %.0f ms  p95 %.0f ms"
              % (ep, acc, len(rows), lat[len(lat) // 2], lat[int(len(lat) * 0.95)]))
    pairs = [(x[0][0], y[0][0]) for x, y in zip(single[eps[0]], single[eps[1]])]

    multi = [r for r in rows if len(r["question"].get("options") or []) >= 2]
    yes = {"type": "yesno", "instructions": "Is the request about deleting something?"}
    for i in range(0, len(multi) - a.batch + 1, a.batch):
        grp = multi[i:i + a.batch]
        qs = [dict(r["question"], type="choice") for r in grp] + [yes]
        x, _ = ask(eps[0], grp[0]["state"], qs)
        y, _ = ask(eps[1], grp[0]["state"], qs)
        pairs.extend(zip(x, y))

    same = sum(1 for x, y in pairs if key(x) == key(y))
    conf = [(x, y) for x, y in pairs if "confidence" in x and "confidence" in y]
    side = sum(1 for x, y in conf if (x["confidence"] >= a.threshold) == (y["confidence"] >= a.threshold))
    dmax = max((abs(x["confidence"] - y["confidence"]) for x, y in conf), default=0.0)
    print("parity %d/%d answers (%d single, %d in batches); same side of %.2f: %d/%d; max |dconf| %.4f"
          % (same, len(pairs), len(rows), len(pairs) - len(rows), a.threshold, side, len(conf), dmax))
    sys.exit(0 if same == len(pairs) else 1)


if __name__ == "__main__":
    main()
