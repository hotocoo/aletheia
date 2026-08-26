"""Does the advice actually help a scheduler? Replay held-out tasks through the kernel's own rule.

`evaluate` reports PR-AUC, calibration and expected cost -- statements about the *classifier*. A
scheduler does not run a classifier; it runs a queue. So this module answers the only question that
justifies putting a forest in a kernel at all: **replaying the same admission stream through the same
selection rule, does consulting the model change what the machine gets done?**

The comparison is deliberately unflattering to the model:

* Both arms run the *identical* selection rule from `kernel-core/src/priosched.rs`
  (`PriorityScheduler::schedule_next` + `risk_prefers`): highest base priority first, FIFO among
  equals. The advised arm differs in exactly one respect -- among tasks of **equal** priority, a
  decisive `low` is preferred over a decisive `elevated`. Priority is never traded for risk, an
  abstention never moves anybody, and no task is ever dropped, delayed past its band, or denied.
  Whatever difference appears therefore comes from tiebreaks and from nothing else.
* Verdicts come from the exported **integer** forest -- the same margins, the same threshold, the
  same conformal band the kernel compares against -- not from XGBoost's float path.
* The stream is the untouched chronological **test** split. Nothing here was fitted, calibrated or
  thresholded on these rows.
* The labels are the trace's own terminal events: `label == 1` means the task really did end in
  eviction, failure, kill or loss. The model does not get to mark its own homework.

The metric is a scheduler metric, not a model metric. With a bounded ready queue of `slots`, tasks
arrive in trace order and one is dispatched per step. A task that will die still runs -- it is
advisory, remember -- but *when* it runs decides how long the work that would have completed sat
behind it. So we report **head-of-line delay for surviving tasks**: the mean number of dispatch steps
a surviving task waits between arriving and being dispatched. Lower is better, and the doomed tasks'
delay is reported alongside it so the trade is visible rather than hidden.
"""

from __future__ import annotations

import json
import time

import numpy as np
import pandas as pd

from . import config as C
from .assemble import load_manifest
from .calibrate import load as load_calibration
from .export import compile_forest, feature_ranges, out_of_range

#: Verdict codes used by the queue. `ABSTAIN` sorts between the two decisive verdicts precisely
#: because the kernel's `risk_prefers` only ever prefers a decisive `low` over a decisive
#: `elevated`: an abstention displaces nobody and is displaced by nobody.
LOW, ABSTAIN, ELEVATED = 0, 1, 2


def verdicts(margin: np.ndarray, oob: np.ndarray, cal: dict) -> np.ndarray:
    """The kernel's three-way verdict, vectorised. Integer comparisons only."""
    thr = int(cal["threshold_margin_fixed"])
    lo = int(cal["abstain_lo_margin_fixed"])
    hi = int(cal["abstain_hi_margin_fixed"])
    v = np.where(margin >= thr, ELEVATED, LOW).astype("int8")
    v[oob | ((margin >= lo) & (margin <= hi))] = ABSTAIN
    return v


def _pick(ready: list[int], prio: np.ndarray, verdict: np.ndarray, advised: bool) -> int:
    """The kernel's selection, on a ready list held in arrival order.

    `priosched.rs` scans the ready set and keeps a challenger when its effective priority is higher,
    or -- with a model resident -- when priorities are EQUAL and `risk_prefers` says so. This is that
    scan, and it is written as a scan rather than as a sort key so it cannot accidentally become a
    total order the kernel does not implement.
    """
    best = 0
    for i in range(1, len(ready)):
        c, inc = ready[i], ready[best]
        if prio[c] > prio[inc]:
            best = i
        elif advised and prio[c] == prio[inc]:
            if verdict[c] == LOW and verdict[inc] == ELEVATED:
                best = i
    return best


def replay(
    prio: np.ndarray,
    verdict: np.ndarray,
    label: np.ndarray,
    slots: int,
    advised: bool,
) -> dict:
    """Run one arm and return its dispatch statistics."""
    n = len(prio)
    ready: list[int] = []
    wait = np.zeros(n, dtype="int64")
    arrive = np.zeros(n, dtype="int64")
    step = 0
    nxt = 0
    while nxt < n or ready:
        while nxt < n and len(ready) < slots:
            arrive[nxt] = step
            ready.append(nxt)
            nxt += 1
        if not ready:
            break
        k = _pick(ready, prio, verdict, advised)
        task = ready.pop(k)
        wait[task] = step - arrive[task]
        step += 1

    survived = label == 0
    doomed = ~survived
    return {
        "dispatches": int(step),
        "survivor_wait_mean": float(wait[survived].mean()) if survived.any() else 0.0,
        "doomed_wait_mean": float(wait[doomed].mean()) if doomed.any() else 0.0,
        "survivor_wait_p95": float(np.percentile(wait[survived], 95)) if survived.any() else 0.0,
        "survivors": int(survived.sum()),
        "doomed": int(doomed.sum()),
    }


def load_stream(rows: int, seed: int) -> pd.DataFrame:
    """A contiguous chronological window of the untouched test split.

    Contiguous, not sampled: a scheduler faces the arrivals the machine actually got, in the order it
    got them, and the cell-pressure features only mean anything in that order.
    """
    man = load_manifest()
    shards = list(man["test"])
    if not shards:
        raise SystemExit("no test shards in the manifest -- run `assemble` first")
    rng = np.random.default_rng(seed)
    start = int(rng.integers(0, len(shards)))
    frames, got = [], 0
    for name in shards[start:] + shards[:start]:
        df = pd.read_parquet(C.SET_DIR / name, columns=["submit_time", *C.FEATURE_NAMES, "label"])
        frames.append(df)
        got += len(df)
        if got >= rows:
            break
    return pd.concat(frames, ignore_index=True).sort_values("submit_time").head(rows)


def _one(profile: str, rows: int, slots: int, seed: int) -> dict:
    """One replay window."""
    forest = compile_forest(profile)
    cal = load_calibration(profile)
    ranges = feature_ranges()

    df = load_stream(rows, seed)
    X = df[list(C.FEATURE_NAMES)].to_numpy(dtype="int32")
    label = df["label"].to_numpy().astype("int8")
    prio = X[:, C.FEATURE_NAMES.index("priority")].astype("int32")

    t0 = time.perf_counter()
    margin = forest.margins(X)
    margin_secs = time.perf_counter() - t0
    v = verdicts(margin, out_of_range(X, ranges), cal)

    # The control arm gets an all-abstain verdict vector, which is exactly what a kernel with no
    # model resident computes -- so "model-free" here is the real model-free path, not a separate
    # simulation of one.
    free = replay(prio, np.full(len(v), ABSTAIN, dtype="int8"), label, slots, advised=False)
    adv = replay(prio, v, label, slots, advised=True)

    census = {
        "low": int((v == LOW).sum()),
        "abstain": int((v == ABSTAIN).sum()),
        "elevated": int((v == ELEVATED).sum()),
    }
    # How good the decisive verdicts were, on the trace's own outcomes.
    dec = v != ABSTAIN
    precision_elevated = float(label[v == ELEVATED].mean()) if (v == ELEVATED).any() else 0.0
    survival_low = float(1.0 - label[v == LOW].mean()) if (v == LOW).any() else 0.0

    improvement = free["survivor_wait_mean"] - adv["survivor_wait_mean"]
    out = {
        "profile": profile,
        "rows": int(len(df)),
        "slots": slots,
        "seed": seed,
        "positive_rate": float(label.mean()),
        "verdict_census": census,
        "decisive_rate": float(dec.mean()),
        "elevated_precision": precision_elevated,
        "low_survival": survival_low,
        "model_free": free,
        "advised": adv,
        "survivor_wait_delta": float(improvement),
        "survivor_wait_delta_pct": float(
            100.0 * improvement / free["survivor_wait_mean"] if free["survivor_wait_mean"] else 0.0
        ),
        "dispatch_count_identical": free["dispatches"] == adv["dispatches"],
        "integer_margin_secs": margin_secs,
    }
    return out


def run(
    profile: str = "kernel",
    rows: int = 60_000,
    slots: int = 64,
    seed: int = 11,
    repeats: int = 3,
) -> dict:
    """Replay `repeats` independent windows and report the spread.

    One window is an anecdote: the arrival mix of five minutes of one cell decides how much room a
    tiebreak has to work in. Repeating over windows chosen by different seeds is what turns "it
    helped here" into "it helped, by this much, in every window we looked at" -- and it is also how a
    window where it did NOT help would become visible instead of being the one we did not run.
    """
    windows = [_one(profile, rows, slots, seed + 7 * i) for i in range(max(1, repeats))]
    deltas = [w["survivor_wait_delta_pct"] for w in windows]
    out = {
        "profile": profile,
        "rows_per_window": rows,
        "slots": slots,
        "windows": windows,
        "survivor_wait_delta_pct_mean": float(np.mean(deltas)),
        "survivor_wait_delta_pct_min": float(np.min(deltas)),
        "survivor_wait_delta_pct_max": float(np.max(deltas)),
        "windows_improved": int(sum(1 for d in deltas if d > 0)),
        "dispatch_count_identical": all(w["dispatch_count_identical"] for w in windows),
    }
    path = C.ARTIFACT_DIR / f"schedsim_{profile}.json"
    path.write_text(json.dumps(out, indent=1))
    for w in windows:
        print(
            f"[schedsim] seed {w['seed']}: base {w['positive_rate']:.4f} positive, "
            f"{w['verdict_census']['low']} low ({w['low_survival']:.4f} survive), "
            f"survivor wait {w['model_free']['survivor_wait_mean']:.3f} -> "
            f"{w['advised']['survivor_wait_mean']:.3f} steps "
            f"({w['survivor_wait_delta_pct']:+.3f}%)",
            flush=True,
        )
    print(
        f"[schedsim] {out['windows_improved']}/{len(windows)} windows improved; "
        f"mean {out['survivor_wait_delta_pct_mean']:+.3f}% "
        f"(min {out['survivor_wait_delta_pct_min']:+.3f}%, "
        f"max {out['survivor_wait_delta_pct_max']:+.3f}%); "
        f"dispatch counts identical: {out['dispatch_count_identical']}",
        flush=True,
    )
    print(f"[schedsim] wrote {path}", flush=True)
    return out
