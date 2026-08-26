"""Aletheia's advised scheduler against the selection rules every major OS family actually ships.

WHAT THIS IS, AND WHAT IT REFUSES TO CLAIM.

`schedsim` answers "does the advice help *Aletheia*?" by changing one thing and holding everything
else fixed. The obvious next question -- "help compared to what, a real operating system?" -- is where
scheduler benchmarks usually stop being honest, so the terms are set out before any number appears.

**Nothing here boots any of these systems, and nothing here measures one.** Booting them would not
answer the question anyway: comparing scheduling *policy* means feeding the same arrivals, with the
same service demands, to each policy, and no two kernels are ever handed the same workload on the same
hardware at the same instant. What this does instead is implement each system's **documented
selection rule** -- the function that answers "which runnable thread runs next" -- in one simulator,
and drive all of them from one trace.

THE ARMS, AND WHAT EACH ONE IS FAITHFUL TO.

*Linux, by the scheduler the distribution actually ships:*

* `linux-cfs` -- Completely Fair Scheduler, 2.6.23 through 6.5 (Debian 12, RHEL 8/9, Ubuntu <= 24.04):
  run the smallest virtual runtime, charged `delta * 1024 / weight` from the real
  `sched_prio_to_weight` table.
* `linux-eevdf` -- Earliest Eligible Virtual Deadline First, 6.6 onward (Fedora, Arch, Ubuntu 24.10+,
  Debian 13): among tasks whose virtual runtime is eligible, the earliest virtual deadline.
* `linux-bore` -- BORE (Burst-Oriented Response Enhancer), shipped by CachyOS, Zen and Liquorix
  kernels: EEVDF plus a burst penalty added to a task's virtual runtime, so a thread that keeps
  consuming whole slices is progressively deprioritised against one that does not.
* `linux-muqss` -- MuQSS / BFS, Con Kolivas' desktop schedulers (Liquorix and the -ck patch set):
  a virtual deadline `now + prio_ratio * slice`, earliest deadline wins, no fairness accounting.
* `linux-rt-fifo` -- `SCHED_FIFO` under PREEMPT_RT (RHEL for Real Time, Ubuntu Pro Realtime, audio
  distributions): strict priority, no timeslice, a running thread keeps the CPU until it yields.

*Other kernels:*

* `windows-nt` -- Windows NT/10/11: 32 priority levels, round-robin within a level, plus the balance
  set manager's anti-starvation boost for a thread that has been ready too long.
* `xnu-macos` -- Darwin/XNU (macOS, iOS): Mach timeshare threads, whose effective priority is the base
  priority minus decayed CPU usage, so a thread that runs sinks and a thread that waits rises.
* `freebsd-ule` -- FreeBSD ULE: an interactivity score plus nice sets a timeshare priority, and the
  runqueue is split into a current and a next queue that swap when current drains, which bounds
  starvation without fairness accounting.
* `solaris-ts` -- Solaris/illumos timeshare class: the `ts_dptbl` dispatch table drops a thread's
  priority when it burns a quantum and raises it after a long wait.
* `zircon-fair` -- Fuchsia's Zircon fair scheduler: weight-proportional virtual finish times.
* `sel4-prio-rr` -- seL4: strict priority, round-robin inside a priority, and deliberately no aging
  at all, because policy is the user level's job.
* `redox-rr` -- Redox, a real Rust microkernel: round-robin over runnable contexts, no priority term.
* `fifo` -- arrival order, no policy whatsoever. The floor.

*And the two arms under test:*

* `aletheia-free` -- Aletheia with no model resident: strict base priority, FIFO among equals.
* `aletheia-advised` -- the same rule plus the ONLY thing the forest is allowed to do: among tasks of
  **equal** priority, prefer a decisive `low` over a decisive `elevated`.

WHAT IS DELIBERATELY MISSING, AND WHY THAT MATTERS.

These are faithful to the selection rules and unfaithful to everything else: no preemption latency, no
cache or NUMA effects, no wakeup placement, no load balancing across cores, no cgroups, no energy
model, no I/O. Two consequences are worth stating rather than burying:

1. A modern scheduler is far more than its pick function, and the omitted parts are exactly where
   decades of engineering went. **None of these numbers say one operating system is faster than
   another.** They say what one rule does to one queue.
2. The trace's tasks are CPU-bound and never sleep, so the arms whose real behaviour is driven by
   *sleep* -- ULE's interactivity score, XNU's usage decay, Solaris' long-wait boost -- degenerate
   here towards usage-penalised round-robin. That is a limitation of the workload, not a verdict on
   those designs, and those three arms should be read with it in mind.

WHAT IS ACTUALLY BEING COMPARED, THEN.

One thing, and it is the thing the model exists for: **none of the other rules know which arrivals are
going to die.** Every kernel above schedules a task that will be evicted in ninety seconds exactly
like one that will run to completion, because nothing in the kernel can tell them apart. Aletheia's
forest can, on data it has never seen. Whether that knowledge is worth anything to a queue is the
question, and the answer is a number rather than an opinion.

THE WORKLOAD. Arrivals, priorities and outcomes come from the untouched chronological test split of
the Google Borg 2019 trace -- real submissions, in the order a real cell received them, labelled with
the terminal event that really happened. The one modelled quantity is **service demand**: the labelled
shards carry no runtime, so each task is given a demand proportional to its `cpu_request`, identically
in every arm. That assumption cannot favour one policy, and it is stated so a reader can attack it.

THE METRIC. **Mean turnaround of tasks that survive** -- dispatch steps between a surviving task
arriving and its last slice completing. Work that completes is the work a machine exists to do. The
doomed tasks' turnaround is reported alongside it, always, so the trade is visible: a policy that
improves one by wrecking the other has improved nothing, and the table will say so.
"""

from __future__ import annotations

import json
import time

import numpy as np

from . import config as C
from .calibrate import load as load_calibration
from .export import compile_forest, feature_ranges, out_of_range
from .schedsim import ABSTAIN, ELEVATED, LOW, load_stream, verdicts

#: Linux's `sched_prio_to_weight`, nice -20..+19 -- what makes a nice level a *proportion* of the CPU
#: rather than a rank. Used verbatim by every Linux arm.
NICE_TO_WEIGHT = (
    88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916,
    9548, 7620, 6100, 4904, 3906, 3121, 2501, 1991, 1586, 1277,
    1024, 820, 655, 526, 423, 335, 272, 215, 172, 137,
    110, 87, 70, 56, 45, 36, 29, 23, 18, 15,
)
NICE_0_LOAD = 1024

#: One dispatch step is one slice; a task needing more comes back for another.
SLICE = 1

#: Windows' balance set manager sweeps for ready threads that have been starved and boosts them. The
#: real interval is wall-clock (~4 s at ~3 quanta); here it is expressed in dispatch steps.
NT_STARVATION_STEPS = 300

#: XNU/Solaris style usage decay: how much accumulated usage survives each dispatch step.
USAGE_DECAY = 0.90

POLICIES = (
    "fifo",
    "redox-rr",
    "sel4-prio-rr",
    "windows-nt",
    "xnu-macos",
    "freebsd-ule",
    "solaris-ts",
    "zircon-fair",
    "linux-cfs",
    "linux-eevdf",
    "linux-bore",
    "linux-muqss",
    "linux-rt-fifo",
    "aletheia-free",
    "aletheia-advised",
)

#: One line per arm, for the printed table and for anyone reading the artifact without the source.
DESCRIPTIONS = {
    "fifo": "arrival order, no policy at all",
    "redox-rr": "Redox: round-robin over runnable contexts",
    "sel4-prio-rr": "seL4: strict priority, round-robin within a priority, no aging",
    "windows-nt": "Windows NT/10/11: 32 levels, RR within level, anti-starvation boost",
    "xnu-macos": "Darwin/XNU: base priority minus decayed CPU usage",
    "freebsd-ule": "FreeBSD ULE: interactivity+nice priority, current/next queue swap",
    "solaris-ts": "Solaris/illumos TS: ts_dptbl, quantum expiry drops priority",
    "zircon-fair": "Fuchsia Zircon: weight-proportional virtual finish time",
    "linux-cfs": "Linux <=6.5 (Debian 12, RHEL 8/9): smallest virtual runtime",
    "linux-eevdf": "Linux 6.6+ (Fedora, Arch, Ubuntu 24.10+): earliest eligible virtual deadline",
    "linux-bore": "CachyOS/Zen/Liquorix BORE: EEVDF plus a burst penalty",
    "linux-muqss": "MuQSS/BFS (-ck, Liquorix): earliest virtual deadline, no fairness accounting",
    "linux-rt-fifo": "PREEMPT_RT SCHED_FIFO (RHEL-RT, audio distros): strict priority, no timeslice",
    "aletheia-free": "Aletheia, no model resident: strict priority, FIFO among equals",
    "aletheia-advised": "Aletheia, forest resident: the same, plus low-over-elevated among EQUALS",
}


def _weights(prio: np.ndarray) -> np.ndarray:
    """Borg priority 0..11 -> a Linux weight.

    Borg's priority rises with importance and Linux's nice *falls* with it, so the mapping is monotone
    and spans the usable middle of the nice range rather than its extremes: priority 0 (the free tier,
    evicted first) becomes nice +10, priority 11 becomes nice -10. Every weighted arm uses the same
    mapping, so it cannot advantage one over another.
    """
    nice = 10 - np.rint(np.clip(prio, 0, 11) * (20.0 / 11.0)).astype("int64")
    return np.asarray(NICE_TO_WEIGHT, dtype="float64")[np.clip(nice + 20, 0, 39)]


def _demand(cpu_request: np.ndarray, max_slices: int = 8) -> np.ndarray:
    """Service demand in slices, from the request the task made. Identical in every arm."""
    q = cpu_request.astype("float64") / max(1.0, float(cpu_request.max()))
    return np.clip(np.rint(1 + q * (max_slices - 1)), 1, max_slices).astype("int64")


class _Entry:
    """One ready task, plus whatever state the policy under test needs to keep about it."""

    __slots__ = ("task", "vruntime", "deadline", "usage", "burst", "arrived", "queue")

    def __init__(self, task: int, v: float, deadline: float, step: int):
        self.task = task
        self.vruntime = v
        self.deadline = deadline
        self.usage = 0.0
        self.burst = 0.0
        self.arrived = step
        self.queue = 0  # ULE: 0 = current, 1 = next


def replay(
    policy: str,
    prio: np.ndarray,
    verdict: np.ndarray,
    demand: np.ndarray,
    weight: np.ndarray,
    label: np.ndarray,
    slots: int,
) -> dict:
    """Drive one policy over the arrival stream and return its turnaround statistics.

    Every arm sees identical arrivals, an identical ready-queue depth and identical service demands.
    The ONLY difference between arms is which ready task is picked next.
    """
    n = len(prio)
    remaining = demand.copy()
    arrive = np.zeros(n, dtype="int64")
    done = np.full(n, -1, dtype="int64")

    ready: list[_Entry] = []
    vmin = 0.0
    step = 0
    nxt = 0
    rr = 0
    ule_swaps = 0

    def _pick() -> int:
        nonlocal ule_swaps
        if policy == "fifo":
            return 0
        if policy == "redox-rr":
            return rr % len(ready)

        if policy == "sel4-prio-rr":
            # Strict priority; round-robin inside a priority means the oldest at the top band.
            top = max(prio[e.task] for e in ready)
            return min(
                (i for i in range(len(ready)) if prio[ready[i].task] == top),
                key=lambda i: ready[i].arrived,
            )

        if policy == "linux-rt-fifo":
            # SCHED_FIFO: strict priority, FIFO within, and no timeslice -- so a task that has begun
            # keeps the CPU. The `burst` field records "this one has started running".
            started = [i for i in range(len(ready)) if ready[i].burst > 0]
            if started:
                top = max(prio[ready[i].task] for i in started)
                cands = [i for i in started if prio[ready[i].task] == top]
                # Nothing of higher priority may be waiting, or it preempts.
                hi = max(prio[e.task] for e in ready)
                if top >= hi:
                    return min(cands, key=lambda i: ready[i].arrived)
            top = max(prio[e.task] for e in ready)
            return min(
                (i for i in range(len(ready)) if prio[ready[i].task] == top),
                key=lambda i: ready[i].arrived,
            )

        if policy == "windows-nt":
            # Highest level first, round-robin within it -- unless the balance set manager finds a
            # thread that has been ready too long, which it boosts ahead of everything.
            starved = [i for i in range(len(ready)) if step - ready[i].arrived > NT_STARVATION_STEPS]
            if starved:
                return min(starved, key=lambda i: ready[i].arrived)
            top = max(prio[e.task] for e in ready)
            return min(
                (i for i in range(len(ready)) if prio[ready[i].task] == top),
                key=lambda i: ready[i].arrived,
            )

        if policy == "xnu-macos":
            # Effective priority = base minus decayed usage: running sinks you, waiting lifts you.
            return max(
                range(len(ready)),
                key=lambda i: (prio[ready[i].task] - ready[i].usage, -ready[i].arrived),
            )

        if policy == "solaris-ts":
            # ts_dptbl: burning a quantum drops you a step; a long wait raises you. Integer steps,
            # unlike XNU's continuous decay -- which is the actual difference between the two.
            def sol(i: int) -> tuple:
                e = ready[i]
                waited = (step - e.arrived) // 100
                return (prio[e.task] - int(e.usage) + min(waited, 5), -e.arrived)

            return max(range(len(ready)), key=sol)

        if policy == "freebsd-ule":
            # Two queues: pick from `current`; when it drains, `next` becomes `current`. Within the
            # current queue, the best timeshare priority wins.
            cur = [i for i in range(len(ready)) if ready[i].queue == 0]
            if not cur:
                for e in ready:
                    e.queue = 0
                ule_swaps += 1
                cur = list(range(len(ready)))
            return max(
                cur, key=lambda i: (prio[ready[i].task] - ready[i].usage * 0.5, -ready[i].arrived)
            )

        if policy in ("linux-cfs",):
            return min(range(len(ready)), key=lambda i: (ready[i].vruntime, ready[i].task))

        if policy in ("linux-eevdf", "linux-bore", "zircon-fair"):
            elig = [i for i in range(len(ready)) if ready[i].vruntime <= vmin + 1e-9]
            pool = elig if elig else range(len(ready))
            return min(pool, key=lambda i: (ready[i].deadline, ready[i].task))

        if policy == "linux-muqss":
            return min(range(len(ready)), key=lambda i: (ready[i].deadline, ready[i].task))

        if policy in ("aletheia-free", "aletheia-advised"):
            advised = policy == "aletheia-advised"
            best = 0
            for i in range(1, len(ready)):
                c, inc = ready[i].task, ready[best].task
                if prio[c] > prio[inc]:
                    best = i
                elif advised and prio[c] == prio[inc]:
                    if verdict[c] == LOW and verdict[inc] == ELEVATED:
                        best = i
            return best

        raise ValueError(f"unknown policy {policy}")

    while nxt < n or ready:
        while nxt < n and len(ready) < slots:
            # A joining task starts at the queue's current virtual time; otherwise it would look
            # infinitely deprived and monopolise every weighted arm. This is `place_entity`'s job.
            e = _Entry(nxt, vmin, vmin + SLICE * NICE_0_LOAD / weight[nxt], step)
            # ULE admits a new task to the current queue; a requeued one goes to next.
            ready.append(e)
            arrive[nxt] = step
            nxt += 1
        if not ready:
            break

        k = _pick()
        e = ready[k]
        task = e.task
        step += 1
        rr += 1
        remaining[task] -= 1

        # Charge the slice. `vruntime` is CFS/EEVDF/Zircon; `usage` is XNU/Solaris/ULE; `burst` is
        # BORE's burst accumulator and doubles as SCHED_FIFO's "has started" flag.
        charge = SLICE * NICE_0_LOAD / weight[task]
        e.burst += SLICE
        e.usage = e.usage * USAGE_DECAY + 1.0
        if policy == "linux-bore":
            # BORE's penalty grows with accumulated burst, so a thread that keeps consuming whole
            # slices is pushed back against one that does not.
            charge *= 1.0 + 0.25 * e.burst
        e.vruntime += charge
        e.deadline = e.vruntime + SLICE * NICE_0_LOAD / weight[task]
        if policy == "freebsd-ule":
            e.queue = 1  # requeued to `next`, which is what bounds starvation here

        if remaining[task] <= 0:
            done[task] = step
            ready.pop(k)
        if ready:
            vmin = min(x.vruntime for x in ready)

    turnaround = done - arrive
    survived = label == 0
    doomed = ~survived
    return {
        "policy": policy,
        "description": DESCRIPTIONS[policy],
        "steps": int(step),
        "completed": int((done >= 0).sum()),
        "survivor_turnaround_mean": float(turnaround[survived].mean()),
        "survivor_turnaround_p95": float(np.percentile(turnaround[survived], 95)),
        "doomed_turnaround_mean": float(turnaround[doomed].mean()),
    }


def run(
    profile: str = "kernel",
    rows: int = 20_000,
    slots: int = 64,
    seed: int = 11,
    repeats: int = 3,
) -> dict:
    forest = compile_forest(profile)
    cal = load_calibration(profile)
    ranges = feature_ranges()

    windows = []
    for w in range(max(1, repeats)):
        df = load_stream(rows, seed + 7 * w)
        X = df[list(C.FEATURE_NAMES)].to_numpy(dtype="int32")
        label = df["label"].to_numpy().astype("int8")
        prio = X[:, C.FEATURE_NAMES.index("priority")].astype("int32")
        cpu = X[:, C.FEATURE_NAMES.index("cpu_request")].astype("int64")

        t0 = time.perf_counter()
        margin = forest.margins(X)
        margin_secs = time.perf_counter() - t0
        v = verdicts(margin, out_of_range(X, ranges), cal)
        # Every arm but the advised one is handed the verdict vector a kernel with NO model resident
        # computes -- all abstain -- so no other policy can accidentally see the forest's opinion.
        blind = np.full(len(v), ABSTAIN, dtype="int8")

        weight = _weights(prio)
        demand = _demand(cpu)
        arms = {}
        for p in POLICIES:
            t1 = time.perf_counter()
            arms[p] = replay(
                p, prio, v if p == "aletheia-advised" else blind, demand, weight, label, slots
            )
            arms[p]["replay_secs"] = time.perf_counter() - t1
        windows.append(
            {
                "seed": seed + 7 * w,
                "rows": int(len(df)),
                "positive_rate": float(label.mean()),
                "slice_steps_total": int(demand.sum()),
                "verdict_census": {
                    "low": int((v == LOW).sum()),
                    "abstain": int((v == ABSTAIN).sum()),
                    "elevated": int((v == ELEVATED).sum()),
                },
                "low_survival": float(1.0 - label[v == LOW].mean()) if (v == LOW).any() else 0.0,
                "integer_margin_secs": margin_secs,
                "arms": arms,
            }
        )

    summary = {}
    for p in POLICIES:
        vals = [w["arms"][p]["survivor_turnaround_mean"] for w in windows]
        dm = [w["arms"][p]["doomed_turnaround_mean"] for w in windows]
        summary[p] = {
            "description": DESCRIPTIONS[p],
            "survivor_turnaround_mean": float(np.mean(vals)),
            "doomed_turnaround_mean": float(np.mean(dm)),
            "windows": [float(x) for x in vals],
        }
    base = summary["aletheia-advised"]["survivor_turnaround_mean"]
    free = summary["aletheia-free"]["survivor_turnaround_mean"]
    for p in POLICIES:
        m = summary[p]["survivor_turnaround_mean"]
        summary[p]["advised_is_better_by_pct"] = float(100.0 * (m - base) / m) if m else 0.0

    steps = [{p: w["arms"][p]["steps"] for p in POLICIES} for w in windows]
    identical = all(len(set(s.values())) == 1 for s in steps)

    out = {
        "profile": profile,
        "rows_per_window": rows,
        "slots": slots,
        "repeats": len(windows),
        "policies": list(POLICIES),
        "summary": summary,
        "windows": windows,
        "total_work_identical_in_every_arm": identical,
        # The honest decomposition: how much of the advised arm's lead over the WORST arm comes from
        # the model, and how much from strict priority scheduling, which any kernel could adopt.
        "model_contribution_pct": float(100.0 * (free - base) / free) if free else 0.0,
    }
    path = C.ARTIFACT_DIR / f"oscompare_{profile}.json"
    path.write_text(json.dumps(out, indent=1))

    print(f"[oscompare] {len(windows)} window(s) of {rows} held-out tasks, ready queue {slots} deep")
    print(
        f"[oscompare] {'policy':<18}{'survivor':>10}{'doomed':>10}{'advised better':>16}   what it is"
    )
    for p in sorted(POLICIES, key=lambda p: -summary[p]["survivor_turnaround_mean"]):
        s = summary[p]
        print(
            f"[oscompare] {p:<18}{s['survivor_turnaround_mean']:>10.3f}"
            f"{s['doomed_turnaround_mean']:>10.3f}{s['advised_is_better_by_pct']:>15.2f}%   "
            f"{s['description']}"
        )
    print(f"[oscompare] total work identical in every arm: {identical}")
    print(
        f"[oscompare] of the advised arm's lead, {out['model_contribution_pct']:.2f} points come "
        f"from the MODEL (advised vs model-free Aletheia); the rest is strict priority scheduling, "
        f"which any kernel could adopt"
    )
    print(f"[oscompare] wrote {path}", flush=True)
    return out
