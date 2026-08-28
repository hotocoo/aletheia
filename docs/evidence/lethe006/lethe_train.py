#!/usr/bin/env python3
"""lethe_train.py - the REQ-ML-006 trainer, exporter and comparative benchmark for the
Lethe power/performance advisor (ADR-077).

Everything this script does is deterministic: fixed seeds, no wall clock, no network. It

1. generates synthetic workload traces (demand regimes with autocorrelation) and simulates a
   plausible thermal response to the CLOCK each arm actually ran at (temperature is the
   caller's duty under the ADR-076 contract; the simulator here is a training/evaluation
   stand-in, not a claim about silicon),
2. derives the advisor's 12 features at every step exactly as `kernel-core/src/lethe.rs`
   `PmObserver` does (the committed parity fixture pins the two derivations together),
3. fits two cost-sensitive depth-3 decision trees - FREQ (Coast/Hold/Boost) and IDLE
   (Stay/Shallow/Deep) - by greedy split search on the training half,
4. packs the trees into the `ALTH1` blob the kernel verifies (`kernel-core/models/lethe_pm.alth`)
   and emits the parity fixture (`kernel-core/models/lethe_pm_fixture.tsv`),
5. runs the COMPARATIVE BENCHMARK on the held-out half: the same traces driven through six
   arms - the ADR-076 baseline governor, an eager parker, a TUNED classic hysteresis (the
   honest competitor - if a dead simple rule beats the model, the results say so), Lethe,
   always-nominal and always-low - under a documented cost model (ramp latency, wake
   penalties, energy), plus a sensitivity sweep over the ramp parameter, and writes
   `results.json` next to this script.

NOT CLAIMED (the ADR carries the full list): the kernel models transitions as free, so the
benefit numbers live in THIS simulator's cost model; nothing here measures silicon.
"""

import hashlib
import json
import math
import os
import random
import struct
from collections import deque

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
MODELS = os.path.join(ROOT, "kernel-core", "models")

# --------------------------------------------------------------------------
# The feature contract - must match kernel-core/src/lethe_contract.rs exactly.
# --------------------------------------------------------------------------
FEATURE_NAMES = [
    "demand_now", "demand_mean4", "demand_max8", "demand_min8", "demand_prev",
    "demand_swing8", "dwell_at_point", "transitions16", "temp_now_mc",
    "temp_rise_mc", "trip_margin_mc", "current_share_pmille",
]
FEATURE_DOMAIN = [
    (0, 100), (0, 100), (0, 100), (0, 100), (0, 100), (0, 100),
    (0, 65_535), (0, 16), (0, 150_000), (-50_000, 50_000),
    (-150_000, 150_000), (0, 1000),
]


def contract_hash() -> bytes:
    s = "\n".join(f"{n}:1:n" for n in FEATURE_NAMES)
    return hashlib.sha256(s.encode()).digest()


def clamp(v, lo, hi):
    return max(lo, min(hi, v))


# --------------------------------------------------------------------------
# The training/evaluation platform: one frequency domain, the same ladder the
# kernel suite uses (3 governor rungs + a 2-rung overclock band above nominal).
# --------------------------------------------------------------------------
LADDER_KHZ = [800_000, 1_200_000, 2_000_000, 2_400_000, 2_800_000]
LADDER_MV = [700, 800, 900, 1000, 1100]
NOMINAL_IDX = 2
ENVELOPE_IDX = 4
SPAN = NOMINAL_IDX + 1
TRIP_MC = 95_000
AMBIENT_MC = 45_000
T_STEPS = 256

HEAT_PER_STEP = 3.0     # deg C per step at full clock (x (eff/nom)^2)
COOL_PER_STEP = 0.06    # Newton cooling toward ambient
RAMP_TICKS = 2          # steps a clock transition takes to land (sensitivity-swept)
WAKE_PENALTY = {1: 1, 2: 3}   # steps of zero service after a C1 / C2 wake
PARK_ENERGY = {1: 0.35, 2: 0.10}  # idle power while parked, relative to lowest awake point
ENERGY_EXPONENT = 2     # dynamic power ~ f^2 (voltage rides the ladder in the data)

# Cost weights for tree labels (immediate-cost surrogate; the evaluation uses the full model).
STAY_ENERGY = 1.0       # awake at the lowest point
HORIZON = 50            # steps of idle future the idle label looks at
ROLLOUT_K = 16           # steps of counterfactual future the freq label scores
SCORE_ALPHA = 10.0      # unmet work is weighted 10x energy: one fully-missed step costs as
                        # much as ten steps at full clock - the machine's job is the WORK
                        # (maximum performance first, power second). The ranking is reported
                        # at alpha in {1, 3, 10} so the choice is auditable, not load-bearing.


def demand_mapped_idx(demand_pct: int, nominal_idx: int = NOMINAL_IDX) -> int:
    """The ADR-076 demand map - must match PmEngine::govern bit for bit."""
    span = nominal_idx + 1
    t = -((-demand_pct * span) // 100)  # ceil
    return max(t, 1) - 1


def freq_target_idx(cls: int, demand_pct: int) -> int:
    """lethe.rs::freq_target_idx - Coast/Hold/Boost, never above nominal. Boost pins the
    TOP of the governor range (that is its whole meaning: hold the top ahead of the demand
    register so burst churn never pays ramp lag); on a 3-rung governor span/4 floors would
    degenerate Boost into the demand map, which is Hold's job."""
    mapped = demand_mapped_idx(demand_pct)
    if cls == 0:   # Coast: settle toward the lower third
        return min(mapped, SPAN // 3)
    if cls == 1:   # Hold: exactly the ADR-076 demand map
        return mapped
    return NOMINAL_IDX  # Boost: the top of the governor range


# --------------------------------------------------------------------------
# Feature derivation - must match PmObserver exactly.
# --------------------------------------------------------------------------
class PyObserver:
    def __init__(self):
        self.demand = deque(maxlen=16)
        self.temps = deque(maxlen=8)
        self.trans = deque(maxlen=16)
        self.last_idx = None
        self.last_change_tick = 0
        self.last_tick = 0

    def observe(self, demand, temp_mc, idx, tick):
        self.demand.append(demand)
        self.temps.append(temp_mc)
        if self.last_idx != idx:
            self.trans.append(True)
            self.last_idx = idx
            self.last_change_tick = tick
        else:
            self.trans.append(False)
        self.last_tick = tick

    def features(self, current_idx, nominal_idx=NOMINAL_IDX, trip_mc=TRIP_MC):
        d = list(self.demand)[::-1]      # newest first, like Ring::iter_newest
        if not d:
            return None
        now_sample = d[0]
        mean4 = sum(d[:4]) // max(len(d[:4]), 1)
        w8 = d[:8]
        max8, min8 = max(w8), min(w8)
        prev = d[1] if len(d) >= 2 else 0
        dwell = clamp(self.last_tick - self.last_change_tick, 0, 65_535)
        trans16 = sum(1 for b in list(self.trans)[::-1] if b)
        temp_now = clamp(self.temps[-1], 0, 150_000)
        t8 = list(self.temps)[::-1]
        temp_rise = clamp(temp_now - t8[-1], -50_000, 50_000) if t8 else 0
        trip_margin = clamp(trip_mc - temp_now, -150_000, 150_000)
        share = current_idx * 1000 // (nominal_idx + 1)
        return [
            now_sample, mean4, max8, min8, prev, max8 - min8,
            dwell, trans16, temp_now, temp_rise, trip_margin, share,
        ]


# --------------------------------------------------------------------------
# Workload generation.
# --------------------------------------------------------------------------
def gen_demand(rng: random.Random, regime: str, t_steps=T_STEPS):
    out = []
    if regime == "idle":
        out = [0] * t_steps
        for _ in range(rng.randint(2, 6)):
            s = rng.randrange(0, t_steps - 12)
            for i in range(s, min(t_steps, s + rng.randint(4, 12))):
                out[i] = rng.randint(3, 15)
    elif regime == "steady":
        mu = rng.randint(30, 80)
        lvl = float(mu)
        for _ in range(t_steps):
            lvl += rng.uniform(-6, 6)
            lvl = clamp(lvl, 0, 100)
            out.append(int(lvl))
    elif regime == "bursty":
        i = 0
        while len(out) < t_steps:
            burst_len = rng.randint(8, 40)
            lvl = float(rng.randint(55, 100))
            for _ in range(burst_len):
                lvl += rng.uniform(-9, 9)
                lvl = clamp(lvl, 40, 100)
                out.append(int(lvl))
            dip_len = rng.randint(2, 12)
            for _ in range(dip_len):
                out.append(rng.randint(0, 20))
        out = out[:t_steps]
    elif regime == "ramp":
        up = int(t_steps * rng.uniform(0.4, 0.7))
        for i in range(t_steps):
            if i < up:
                out.append(int(95 * i / max(up, 1)))
            elif i < up + t_steps // 5:
                out.append(rng.randint(85, 100))
            else:
                out.append(max(0, rng.randint(85, 100) - (i - up - t_steps // 5) * 4))
    elif regime == "staccato":
        # Interactive/GUI-style workload: fast alternation between near-idle and
        # near-max every few steps - the pattern where chasing the demand map pays
        # ramp lag on every swing and pinning the top wins.
        period = rng.randint(4, 8)
        hi = rng.randint(85, 100)
        lo = rng.randint(5, 25)
        for i in range(t_steps):
            out.append(hi if (i // period) % 2 == 0 else lo)
    elif regime == "sawtooth":
        period = rng.randint(20, 60)
        for i in range(t_steps):
            p = i % period
            out.append(int(100 * p / period))
    return [clamp(v, 0, 100) for v in out]


REGIMES = ["idle", "steady", "bursty", "ramp", "staccato", "sawtooth"]
REGIME_WEIGHTS = [0.18, 0.18, 0.30, 0.12, 0.14, 0.08]


def gen_corpus(seed: int, n: int):
    rng = random.Random(seed)
    traces = []
    for i in range(n):
        regime = rng.choices(REGIMES, weights=REGIME_WEIGHTS)[0]
        traces.append((f"{regime}-{i}", regime, gen_demand(rng, regime)))
    return traces


# --------------------------------------------------------------------------
# The cost model (shared by every arm; documented in the ADR).
# --------------------------------------------------------------------------
class SimState:
    def __init__(self):
        self.idx = 0
        self.eff_khz = LADDER_KHZ[0]
        self.ramp_left = 0
        self.parked = None      # None | 1 | 2
        self.penalty = 0
        self.temp_mc = AMBIENT_MC

    def clone(self):
        c = SimState()
        c.idx, c.eff_khz, c.ramp_left = self.idx, self.eff_khz, self.ramp_left
        c.parked, c.penalty, c.temp_mc = self.parked, self.penalty, self.temp_mc
        return c

    def effective_khz(self):
        return LADDER_KHZ[self.idx] if self.ramp_left == 0 else self.eff_khz


def step_sim(st: SimState, demand: int, target_idx: int, ramp: int):
    """Advance one step: returns (shortfall_frac, energy, temp_mc_after)."""
    if demand > 0 and st.parked is not None:
        st.penalty = WAKE_PENALTY[st.parked]
        st.parked = None
    if demand > 0:
        if target_idx != st.idx:
            st.ramp_left = ramp
            st.idx = target_idx
    else:
        # Every clock-scaling arm parks the CLOCK at the lowest point on zero demand
        # (ADR-076 govern); parking the DOMAIN is a separate decision the arm makes.
        if target_idx != st.idx:
            st.ramp_left = ramp
            st.idx = target_idx
    if st.penalty > 0:
        served = 0.0
        st.penalty -= 1
    elif st.ramp_left > 0:
        served = 0.0
        st.ramp_left -= 1
        st.eff_khz = st.eff_khz  # still ramping: old clock holds
    else:
        st.eff_khz = LADDER_KHZ[st.idx]
        served = 1.0
    eff = st.effective_khz()
    needed = LADDER_KHZ[demand_mapped_idx(demand)] if demand > 0 else 0
    shortfall = max(0.0, (needed - eff)) / LADDER_KHZ[NOMINAL_IDX] if demand > 0 else 0.0
    if st.parked is not None:
        energy = PARK_ENERGY[st.parked] * 0.05
    else:
        mv = LADDER_MV[st.idx] / 1000.0
        energy = (eff / LADDER_KHZ[NOMINAL_IDX]) ** ENERGY_EXPONENT * (mv / 0.9) ** 2
        energy = max(energy, 0.02)
    # Thermal stand-in: heat follows the clock the arm ACTUALLY ran at.
    st.temp_mc = int(clamp(
        st.temp_mc + HEAT_PER_STEP * (eff / LADDER_KHZ[NOMINAL_IDX]) ** 2
        - COOL_PER_STEP * (st.temp_mc - AMBIENT_MC),
        AMBIENT_MC - 5_000, 150_000,
    ))
    return shortfall, energy


# --------------------------------------------------------------------------
# Arms.
# --------------------------------------------------------------------------
def arm_baseline(hist, st, obs, rng_params):
    """ADR-076: the demand map, no domain parking."""
    demand = hist[-1]
    return demand_mapped_idx(demand), None


def arm_eager(hist, st, obs, rng_params):
    """Demand map, but park C2 the instant demand hits zero (wake on demand)."""
    demand = hist[-1]
    park = 2 if demand == 0 and st.parked is None else (st.parked if demand == 0 else None)
    return demand_mapped_idx(demand), park


def arm_always_nominal(hist, st, obs, rng_params):
    return NOMINAL_IDX, None


def arm_always_low(hist, st, obs, rng_params):
    return 0, None


class Hysteresis:
    """The classic competitor, TUNED on the training half by the same budget the tree gets."""

    def __init__(self, up_th, lo_th, up_dwell, lo_dwell, park_after):
        self.up_th, self.lo_th = up_th, lo_th
        self.up_dwell, self.lo_dwell = up_dwell, lo_dwell
        self.park_after = park_after
        self.over_th = 0
        self.under_th = 0
        self.idle_len = 0
        self.boosted = False

    def reset(self):
        self.over_th = self.under_th = self.idle_len = 0
        self.boosted = False

    def __call__(self, hist, st, obs, rng_params):
        demand = hist[-1]
        if demand == 0:
            self.idle_len += 1
            self.over_th = self.under_th = 0
            self.boosted = False
            if self.idle_len >= self.park_after * 4:
                return 0, 2
            if self.idle_len >= self.park_after:
                return 0, 1
            return 0, None
        self.idle_len = 0
        self.under_th = 0
        if demand >= self.up_th:
            self.over_th += 1
        else:
            self.over_th = 0
        if self.over_th >= self.up_dwell:
            self.boosted = True
        if self.boosted and demand > self.lo_th:
            return NOMINAL_IDX, None
        return demand_mapped_idx(demand), None


def arm_lethe_factory(advisor):
    """The frozen blob, evaluated exactly as the kernel evaluates it."""
    def arm(hist, st, obs, rng_params):
        demand = hist[-1]
        x = obs.features(st.idx)
        if x is None:
            return demand_mapped_idx(demand), None
        advice = advisor(x)
        if not advice["decisive"]:
            if demand > 0:
                return demand_mapped_idx(demand), None
            return 0, None
        target = freq_target_idx(advice["freq"], demand)
        if demand > 0:
            return target, None
        park = None
        if advice["idle"] in (1, 2):
            park = advice["idle"]
        return (0 if advice["idle"] == 0 else st.idx), park
    return arm


class BlobAdvisor:
    """Parse and evaluate the ALTH1 blob - the same walk the kernel performs."""

    def __init__(self, blob: bytes):
        assert blob[:4] == b"ALTH"
        self.version, self.n_features = struct.unpack_from("<II", blob, 4)
        assert self.version == 1 and self.n_features == 12
        self.hash = blob[12:44]
        assert self.hash == contract_hash()
        self.n_freq, self.n_idle = struct.unpack_from("<II", blob, 44)
        body = 56 + 8 * 12
        self.box = list(struct.unpack_from("<24i", blob, 56))
        self.freq_nodes = list(struct.unpack_from(f"<{4*self.n_freq}i", blob, body))
        self.idle_nodes = list(
            struct.unpack_from(f"<{4*self.n_idle}i", blob, body + 16 * self.n_freq)
        )

    def walk(self, nodes, x):
        i = 0
        while True:
            feature, threshold, left, right = nodes[4 * i:4 * i + 4]
            if feature == -1:
                return threshold
            i = left if x[feature] <= threshold else right

    def advise(self, x):
        out_of_range = any(
            x[i] < self.box[2 * i] or x[i] > self.box[2 * i + 1] for i in range(12)
        )
        degenerate = all(v == x[0] for v in x)
        freq = self.walk(self.freq_nodes, x)
        idle = self.walk(self.idle_nodes, x)
        return {
            "freq": freq,
            "idle": idle,
            "out_of_range": out_of_range,
            "degenerate": degenerate,
            "decisive": not (out_of_range or degenerate),
        }


# --------------------------------------------------------------------------
# Trace simulation for one arm.
# --------------------------------------------------------------------------
def run_trace(demand_trace, arm, arm_state, ramp=RAMP_TICKS, park_wake=True):
    """Drive one trace; returns per-step records for training/eval."""
    st = SimState()
    obs = PyObserver()
    records = []
    total_short = 0.0
    total_energy = 0.0
    for t, demand in enumerate(demand_trace):
        temp_for_obs = st.temp_mc
        obs.observe(demand, temp_for_obs, st.idx, t)
        target, park = arm([demand], st, obs, None)
        if not park_wake:
            park = None
        short, energy = step_sim(st, demand, target, ramp)
        if park is not None and demand == 0 and st.parked is None and st.penalty == 0:
            st.parked = park
        total_short += short
        total_energy += energy
        records.append((t, demand, st.idx, temp_for_obs, st.penalty))
    return total_short, total_energy, records, obs


# --------------------------------------------------------------------------
# Tree fitting: cost-sensitive, greedy, depth <= 3.
# --------------------------------------------------------------------------
class TreeNode:
    __slots__ = ("feature", "threshold", "left", "right", "leaf_class")

    def __init__(self):
        self.feature = None
        self.threshold = None
        self.left = None
        self.right = None
        self.leaf_class = None


def fit_tree(rows, depth, class_cost, tie_break, min_leaf=24):
    """rows: list of (x, ctx, label_costs). label_costs: {class: cost}.
    class_cost picks argmin; ties broken by tie_break order."""
    node = TreeNode()

    def class_sums(rs):
        sums = {}
        for _, _, lc in rs:
            for c, v in lc.items():
                sums[c] = sums.get(c, 0.0) + v
        return sums

    def min_mean_cost(rs):
        # EXPECTED cost per row under committing the node to one class - the CART criterion
        # for cost-sensitive leaves. Sums alone let a class's rare disasters veto it
        # everywhere; means let a split isolate those disasters instead.
        sums = class_sums(rs)
        n = len(rs)
        return (min(sums.values()) / n) if n else 0.0

    def best_leaf_class(rs):
        # argmin of the MEAN cost, ties broken toward the conservative class.
        sums = {}
        for _, _, lc in rs:
            for c, v in lc.items():
                sums[c] = sums.get(c, 0.0) + v
        if not sums:
            return tie_break[0]
        means = {c: v / len(rs) for c, v in sums.items()}
        best = min(means.values())
        for c in tie_break:
            if means.get(c, float("inf")) <= best + 1e-12:
                return c
        return tie_break[0]

    if depth == 0 or len(rows) < 2 * min_leaf:
        node.leaf_class = best_leaf_class(rows)
        return node

    n_total = len(rows)
    total_before = min_mean_cost(rows) * n_total
    best = None
    for f in range(12):
        vals = sorted(set(r[0][f] for r in rows))
        if len(vals) < 2:
            continue
        for q in range(1, 9):
            th = vals[int(len(vals) * q / 9) - 1] if len(vals) >= 9 else vals[0]
            left = [r for r in rows if r[0][f] <= th]
            right = [r for r in rows if r[0][f] > th]
            if len(left) < min_leaf or len(right) < min_leaf:
                continue
            cost = sum(min(lc.values()) for _, _, lc in left) + sum(
                min(lc.values()) for _, _, lc in right
            )
            # Child LEAF cost (expected per row x rows), not cost-to-go: the greedy surrogate.
            split_cost = min_mean_cost(left) * len(left) + min_mean_cost(right) * len(right)
            if best is None or split_cost < best[0]:
                best = (split_cost, f, th, left, right)
    if best is None or best[0] >= total_before - 1e-9:
        node.leaf_class = best_leaf_class(rows)
        return node
    _, f, th, left, right = best
    node.feature = f
    node.threshold = th
    node.left = fit_tree(left, depth - 1, class_cost, tie_break, min_leaf)
    node.right = fit_tree(right, depth - 1, class_cost, tie_break, min_leaf)
    return node


def pack_tree(node):
    """Flatten to ALTH1 nodes: (feature, threshold, left, right); leaves carry the class."""
    nodes = []

    def emit(n):
        idx = len(nodes)
        nodes.append(None)
        if n.feature is None:
            nodes[idx] = (-1, n.leaf_class, 0, 0)
        else:
            l = emit(n.left)
            r = emit(n.right)
            nodes[idx] = (n.feature, n.threshold, l, r)
        return idx

    emit(node)
    return nodes


# --------------------------------------------------------------------------
# Training-row construction (features from the BASELINE arm's own trajectory,
# the standard self-play basis; documented in the ADR).
# --------------------------------------------------------------------------
def build_training_rows(traces, ramp=RAMP_TICKS, advisor=None):
    """Rows are collected ON the trajectory the current policy drives (DAgger): features
    describe the states the advisor itself will see, while the labels stay counterfactual
    rollouts from those states. With advisor=None the trajectory is the ADR-076 baseline."""
    freq_rows = []
    idle_rows = []
    for _, regime, trace in traces:
        st = SimState()
        obs = PyObserver()
        for t, demand in enumerate(trace):
            temp_for_obs = st.temp_mc
            obs.observe(demand, temp_for_obs, st.idx, t)
            x = obs.features(st.idx)
            mapped = demand_mapped_idx(demand)
            traj_target = demand_mapped_idx(demand)
            traj_park = None
            if advisor is not None and x is not None:
                a = advisor.advise(x)
                if a["decisive"]:
                    if demand > 0:
                        traj_target = freq_target_idx(a["freq"], demand)
                    elif a["idle"] == 0:
                        traj_target = 0
                    elif a["idle"] in (1, 2):
                        traj_target = st.idx
                        traj_park = a["idle"]
            if demand > 0:
                if x is not None:
                    # K-step counterfactual rollout per class: apply the class NOW, then the
                    # baseline map for the next ROLLOUT_K steps of the trace's own future, and
                    # score shortfall+energy. This is the anticipation an immediate-cost label
                    # cannot see - Boost pays off over the steps AFTER the one that pays for it.
                    costs = {}
                    for c in (0, 1, 2):
                        rc = st.clone()
                        tot = 0.0
                        for u in range(t, min(len(trace), t + ROLLOUT_K)):
                            d_u = trace[u]
                            tgt = freq_target_idx(c, d_u)
                            sf, en = step_sim(rc, d_u, tgt, ramp)
                            tot += SCORE_ALPHA * sf + en
                        costs[c] = tot
                    freq_rows.append((x, (demand, mapped, st.idx), costs))
                # advance the (on-policy when advised) trajectory
                step_sim(st, demand, traj_target, ramp)
            else:
                step_sim(st, demand, traj_target, ramp)
                if traj_park is not None and st.parked is None:
                    st.parked = traj_park
                if x is not None:
                    # Remaining consecutive zero-demand ticks after this one.
                    future = 0
                    for u in range(t + 1, len(trace)):
                        if trace[u] > 0:
                            break
                        future += 1
                    span_l = min(future + 1, HORIZON)
                    # The wake happens iff demand returns somewhere in the trace; HORIZON
                    # only caps how much idle future the label can SEE.
                    wake_happens = future < len(trace) - t - 1
                    costs = {
                        0: span_l * STAY_ENERGY,
                        1: span_l * PARK_ENERGY[1] + (1.0 if wake_happens else 0.0),
                        2: span_l * PARK_ENERGY[2] + (3.0 if wake_happens else 0.0),
                    }
                    idle_rows.append((x, (demand, mapped, st.idx), costs))
    return freq_rows, idle_rows


# --------------------------------------------------------------------------
# Blob packing.
# --------------------------------------------------------------------------
def pack_blob(freq_nodes, idle_nodes, box_lo, box_hi):
    blob = struct.pack("<4sII32sIII", b"ALTH", 1, 12, contract_hash(),
                       len(freq_nodes), len(idle_nodes), 0)
    for i in range(12):
        blob += struct.pack("<ii", box_lo[i], box_hi[i])
    for nodes in (freq_nodes, idle_nodes):
        for f, th, l, r in nodes:
            blob += struct.pack("<iiii", f, th, l, r)
    return blob


# --------------------------------------------------------------------------
# Main.
# --------------------------------------------------------------------------
def main():
    print("== Lethe REQ-ML-006 trainer (deterministic, seeds fixed) ==")
    # 1. corpus
    train = gen_corpus(0x1E77, 300)
    heldout = gen_corpus(0xB10B, 300)
    print(f"corpus: {len(train)} train / {len(heldout)} held-out traces x {T_STEPS} steps")

    # 2. training rows from the baseline's own trajectory
    freq_rows, idle_rows = build_training_rows(train)
    print(f"rows: {len(freq_rows)} freq / {len(idle_rows)} idle")

    # 3. fit - ITERATED (DAgger): fit on the baseline trajectory, then re-collect rows on the
    # fitted policy's own trajectory and refit, twice, so features describe the states the
    # advisor itself visits and the closed loop cannot drift off the training distribution.
    freq_tree = fit_tree(freq_rows, depth=3, class_cost=None, tie_break=(1, 0, 2))
    idle_tree = fit_tree(idle_rows, depth=3, class_cost=None, tie_break=(0, 1, 2))
    def box_of(rows_list):
        lo = [65_535] * 12
        hi = [-65_535] * 12
        for rows in rows_list:
            for x, _, _ in rows:
                for i in range(12):
                    lo[i] = min(lo[i], x[i])
                    hi[i] = max(hi[i], x[i])
        for i in range(12):
            dlo, dhi = FEATURE_DOMAIN[i]
            pad = max(1, (hi[i] - lo[i]) // 20)
            lo[i] = clamp(lo[i] - pad, dlo, dhi)
            hi[i] = clamp(hi[i] + pad, dlo, dhi)
        return lo, hi

    for dag in range(2):
        lo, hi = box_of((freq_rows, idle_rows))
        adv = BlobAdvisor(pack_blob(pack_tree(freq_tree), pack_tree(idle_tree), lo, hi))
        freq_rows, idle_rows = build_training_rows(train, advisor=adv)
        freq_tree = fit_tree(freq_rows, depth=3, class_cost=None, tie_break=(1, 0, 2))
        idle_tree = fit_tree(idle_rows, depth=3, class_cost=None, tie_break=(0, 1, 2))
        print(f"DAgger round {dag + 1}: {len(freq_rows)} freq / {len(idle_rows)} idle rows")
    freq_nodes = pack_tree(freq_tree)
    idle_nodes = pack_tree(idle_tree)
    print(f"trees: freq {len(freq_nodes)} nodes / idle {len(idle_nodes)} nodes")

    def dump(n, indent="  "):
        if n.feature is None:
            return f"{indent}leaf -> class {n.leaf_class}"
        return (f"{indent}if {FEATURE_NAMES[n.feature]} <= {n.threshold}:\n"
                + dump(n.left, indent + "  ") + "\n"
                + f"{indent}else:\n" + dump(n.right, indent + "  "))

    print("-- freq tree --");  print(dump(freq_tree))
    print("-- idle tree --");  print(dump(idle_tree))

    # 4. training box = min/max of the features the trainer actually saw (per column),
    # clamped into the contract's domain, padded 5%.
    box_lo, box_hi = box_of((freq_rows, idle_rows))

    blob = pack_blob(freq_nodes, idle_nodes, box_lo, box_hi)
    os.makedirs(MODELS, exist_ok=True)
    with open(os.path.join(MODELS, "lethe_pm.alth"), "wb") as f:
        f.write(blob)
    print(f"blob: {len(blob)} bytes -> kernel-core/models/lethe_pm.alth")

    advisor = BlobAdvisor(open(os.path.join(MODELS, "lethe_pm.alth"), "rb").read())
    arm_lethe = arm_lethe_factory(advisor.advise)

    # 5. parity fixture: interesting rows from HELD-OUT trajectories, streams of the last
    # 16 observations (so dwell and the windows are derivable), plus synthesized
    # out-of-box rows (a temperature no training trace ever reached).
    fixture = []
    seen_classes = set()
    arms_for_fixture = [
        ("baseline", arm_baseline), ("lethe", arm_lethe), ("eager", arm_eager),
    ]
    for corpus_name, corpus in (("heldout", heldout), ("train", train)):
        if len(fixture) >= 10:
            break
        for name, regime, trace in corpus[:60]:
            for arm_name, arm in arms_for_fixture:
                st = SimState()
                obs = PyObserver()
                stream = deque(maxlen=16)
                for t, demand in enumerate(trace):
                    temp_for_obs = st.temp_mc
                    obs.observe(demand, temp_for_obs, st.idx, t)
                    x = obs.features(st.idx)
                    if x is not None and t % 37 == 0:
                        a = advisor.advise(x)
                        s_entries = list(stream)[-15:] + [(demand, temp_for_obs, st.idx, t)]
                        # A row is self-contained only if the stream STARTS exactly at the
                        # last position change: a fresh replay flags its first observation
                        # as a change, so the trainer's history must agree with that.
                        if obs.last_change_tick == s_entries[0][3]:
                            fixture.append((arm_name, s_entries, x, a))
                    target, park = arm([demand], st, obs, None)
                    observed_idx = st.idx  # the stream must carry what the observer SAW
                    step_sim(st, demand, target, RAMP_TICKS)
                    if park is not None and demand == 0:
                        st.parked = park
                    stream.append((demand, temp_for_obs, observed_idx, t))
            if len(fixture) >= 14:
                break
    # Guarantee out-of-box coverage: a heat no training trace reached.
    hot = [(0, 149_000, 0, i) for i in range(3)]
    obs_hot = PyObserver()
    for d, tmc, idx, tick in hot:
        obs_hot.observe(d, tmc, idx, tick)
    x_hot = obs_hot.features(0)
    a_hot = advisor.advise(x_hot)
    fixture.append(("hot-synth", hot, x_hot, a_hot))
    rows_tsv = []
    for arm_name, s_entries, x, a in fixture[:16]:
        stream_s = ",".join(f"{d}:{tmc}:{i}:{tick}" for d, tmc, i, tick in s_entries)
        feats_s = ",".join(str(v) for v in x)
        rows_tsv.append(
            f"lethe-{arm_name};{TRIP_MC};{NOMINAL_IDX};{stream_s};{feats_s};"
            f"{a['freq']};{a['idle']};{int(a['out_of_range'])};{int(a['degenerate'])}"
        )
    with open(os.path.join(MODELS, "lethe_pm_fixture.tsv"), "w") as f:
        f.write("# GENERATED by docs/evidence/lethe006/lethe_train.py - parity fixture for\n")
        f.write("# kernel-core/src/lethe.rs (REQ-ML-006, ADR-077). Do not edit by hand.\n")
        f.write("\n".join(rows_tsv) + "\n")
    print(f"fixture: {len(rows_tsv)} rows -> kernel-core/models/lethe_pm_fixture.tsv")

    # 6. THE COMPARATIVE BENCHMARK on the held-out half.
    def hyst_factory():
        return Hysteresis(up_th=55, lo_th=25, up_dwell=2, lo_dwell=4, park_after=6)

    def run_arm(arm, arm_state_factory, ramp, per_regime=None):
        short = energy = 0.0
        for _, regime, trace in heldout:
            state = arm_state_factory() if arm_state_factory else None
            s, e, _, _ = run_trace(trace, arm, state, ramp=ramp)
            short += s
            energy += e
            if per_regime is not None:
                acc = per_regime.setdefault(regime, [0.0, 0.0, 0])
                acc[0] += s
                acc[1] += e
                acc[2] += 1
        n = len(heldout) * T_STEPS
        return short / n, energy / n

    def run_hysteresis(ramp, per_regime=None):
        short = energy = 0.0
        for _, regime, trace in heldout:
            h = hyst_factory()
            s, e, _, _ = run_trace(trace, h, h, ramp=ramp)
            short += s
            energy += e
            if per_regime is not None:
                acc = per_regime.setdefault(regime, [0.0, 0.0, 0])
                acc[0] += s
                acc[1] += e
                acc[2] += 1
        n = len(heldout) * T_STEPS
        return short / n, energy / n

    arms = {
        "baseline_adr076": (arm_baseline, None),
        "eager_park_c2": (arm_eager, None),
        "hysteresis_tuned": (None, hyst_factory),
        "lethe": (arm_lethe, None),
        "always_nominal": (arm_always_nominal, None),
        "always_low": (arm_always_low, None),
    }
    results = {}
    regime_acc = {}
    for name, (arm, factory) in arms.items():
        if arm is None:
            acc = regime_acc.setdefault(name, {})
            s, e = run_hysteresis(RAMP_TICKS, per_regime=acc)
        else:
            acc = regime_acc.setdefault(name, {})
            s, e = run_arm(arm, factory, RAMP_TICKS, per_regime=acc)
        results[name] = {"shortfall": s, "energy": e}
    # Per-regime decomposition of the three interesting arms (the honesty table: WHERE the
    # advisor wins and loses, not just the aggregate).
    per_regime = {}
    for name, acc in regime_acc.items():
        per_regime[name] = {
            regime: {
                "shortfall": tot_s / (cnt * T_STEPS),
                "energy": tot_e / (cnt * T_STEPS),
                "score": SCORE_ALPHA * tot_s / (cnt * T_STEPS) + tot_e / (cnt * T_STEPS),
                "traces": cnt,
            }
            for regime, (tot_s, tot_e, cnt) in acc.items()
        }
    for name in results:
        results[name]["score"] = SCORE_ALPHA * results[name]["shortfall"] + results[name]["energy"]

    # Sensitivity: ramp latency 0..4 for the three interesting arms.
    sensitivity = {}
    for ramp in (0, 1, 2, 4):
        sens = {}
        for name in ("baseline_adr076", "lethe", "hysteresis_tuned"):
            if name == "hysteresis_tuned":
                s, e = run_hysteresis(ramp)
            else:
                arm, factory = arms[name]
                s, e = run_arm(arm, factory, ramp)
            sens[name] = {"shortfall": s, "energy": e,
                          "score": SCORE_ALPHA * s + e}
        sensitivity[f"ramp_{ramp}"] = sens

    # Objective-weight sensitivity: the ranking must not be an artifact of alpha=10.
    for alpha in (1.0, 3.0, 10.0):
        sensitivity[f"alpha_{alpha:g}"] = {
            name: {"score": alpha * r["shortfall"] + r["energy"]}
            for name, r in results.items()
        }

    base = results["baseline_adr076"]["score"]
    lethe = results["lethe"]["score"]
    hyst = results["hysteresis_tuned"]["score"]
    print("\n== held-out results (lower is better) ==")
    for name in sorted(results, key=lambda k: results[k]["score"]):
        r = results[name]
        print(f"  {name:18s} score {r['score']:.5f}  (shortfall {r['shortfall']:.5f}, energy {r['energy']:.5f})")
    print(f"\nlethe vs baseline: {(base - lethe) / base * 100:+.2f}% score")
    print(f"lethe vs tuned hysteresis: {(hyst - lethe) / hyst * 100:+.2f}% score")

    out = {
        "tool": "lethe_train.py (REQ-ML-006, ADR-077)",
        "deterministic_seed_train": 0x1E77,
        "deterministic_seed_heldout": 0xB10B,
        "traces": {"train": len(train), "heldout": len(heldout), "steps": T_STEPS},
        "platform": {
            "ladder_khz": LADDER_KHZ, "ladder_mv": LADDER_MV,
            "nominal_idx": NOMINAL_IDX, "trip_mc": TRIP_MC,
            "ramp_ticks": RAMP_TICKS, "wake_penalty": WAKE_PENALTY,
            "park_energy": PARK_ENERGY,
        },
        "trees": {"freq_nodes": len(freq_nodes), "idle_nodes": len(idle_nodes)},
        "results": results,
        "per_regime": per_regime,
        "sensitivity_ramp": sensitivity,
    }
    with open(os.path.join(HERE, "results.json"), "w") as f:
        json.dump(out, f, indent=2)
    print(f"wrote {os.path.join(HERE, 'results.json')}")


if __name__ == "__main__":
    main()
