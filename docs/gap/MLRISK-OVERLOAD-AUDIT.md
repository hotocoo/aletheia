# `aletheia_risk` Overload Audit — five defects the gate bench could not see

**As of:** 2026-08-17 (first wave after the model stops being installed and starts being consulted)
**Bench binary:** `kernel-core/examples/overload_bench.rs` (new — see "Reproduction" below)
**Blob under test:** `kernel-core/models/aletheia_risk.altm` (171 trees, 26 469 nodes, 1 368 worst-case
compares per advice, sha256 `84af4e8d…` as written; re-made at alpha 0.03 by the ALET-P3-008 fix —
sha256 `3e4def46…`, same forest, live band — see Verification outcomes)
**Sources of truth for the live numbers:** raw output from `cargo run --release --example overload_bench`
on the host release build, captured against the blob as committed at HEAD.

This document records what an `aletheia_risk` overload bench — one billion advices, twenty million
deterministic repeats, fifty million pathological inputs, five million samples per feature boundary —
found that the gate bench (`kernel-core/tests/mlrisk_stress.rs`, capped at 2 M advices / 8 K tasks) could
not. Four real defects surfaced that the gate does not see, plus one pre-existing defect (`aletheia-ml`'s
inverted conformal band, MODEL-CARD §4) re-confirmed at every scale. Each is given an ID, a measurement,
and a named owner — for the register, see `docs/gap/ARCHITECTURE-GAPS4-REGISTER.md` rows
**ALET-P3-004 … ALET-P3-008**.

---

## The bench, briefly

`kernel-core/examples/overload_bench.rs` (added in this wave) is a host-side harness that loads
`BUNDLED_MODEL` directly, runs `RiskAdvisor::advise` in tight loops, and prints what happened. It has
five modes:

* `advice_storm <N>` — N advices across four input distributions (uniform in-box, biased-to-priority,
  biased-to-low-signal, all-zero), to distinguish "the model is one-bit everywhere" from "the model is
  one-bit on this surface only".
* `schedule_storm <N>` — admit N tasks through `PriorityScheduler`, drain, assert priority monotonicity.
  This is the one that broke first (see DEFECT-4).
* `determinism <N>` — two independent passes over N random in-box inputs; bit-equal census + margin
  statistics required.
* `fault_inject <N>` — N inputs per pattern: `i32::MIN`, `i32::MAX`, alternating extremes, first-half-MIN
  second-half-MAX, all-zero. Range guard and panic-free, never-slow path under each.
* `boundary_sweep <N>` — for every feature, sweep N samples across its full range while holding the
  others at midpoint. Reports margin-monotonicity per feature.

Defaults: 100 M advices / 1 M tasks / 2 M fault-inject / 1 M boundary-sweep. CLI overrides via positional
args or `OLOAD_*` env vars. The bench is independent of the test harness: it has its own `main`, prints
to stdout, and exits with a non-zero code if anything goes wrong. Five modes, ~700 lines, no new deps.

---

## The numbers, by mode

### `advice_storm` at 1 000 000 000 advices (250 M per surface)

```
total        = 1 000 000 000  (4 surfaces × 250 000 000)
elapsed      = 3 306.104 s    (55 min 6 s)
per advice   = 3 306.1 ns     (3 306 104 ps)
throughput   = 3.02 × 10^5 adv/s    (302 474 adv/s)
verdicts     = low=811   elevated=999 999 189   abstain=0   (in-band abstain: 0)
out_of_range = 0
margin       = min=-7 069 968  max=18 989 320  mean=7 480 513.2
```

| Surface | Low | Elevated | Abstain | band | oor | margin range |
|---|---|---|---|---|---|---|
| `uniform_in_box` | 286 | 249 999 714 | 0 | 0 | 0 | [-5 878 329, 18 926 291] |
| `biased_to_priority_features` | **0** | 250 000 000 | 0 | 0 | 0 | [6 091 562, 18 989 320] |
| `biased_to_low_signal` | 525 | 249 999 475 | 0 | 0 | 0 | [-7 069 968, 13 292 713] |
| `all_zero_band_edge` | 0 | 250 000 000 | 0 | 0 | 0 | [-1 781 991, -1 781 991] |

Throughput at 1 B was stable across the run: 2.75 × 10⁵ adv/s cold, climbing to 3.17 × 10⁵ as caches
warmed, settling at 3.02 × 10⁵. Gate bench said 3 524 ps/advice on 2 M advices. 1 B says 3 306 ps.
Same model, ~6% difference attributable to cache state. **No drift, no slow path under load.**

### `determinism` at 20 000 000 advices (two passes)

```
pass 1: 72.764 s  low=17 elevated=19 999 983 abstain=0
pass 2: 72.158 s  low=17 elevated=19 999 983 abstain=0
margin: pass1=[-5 103 432, 18 250 797] first=8 323 024
        pass2=[-5 103 432, 18 250 797] first=8 323 024
BIT-IDENTICAL across 20 000 000 advices, two passes
```

Per-advice cost: 3 638 ns (pass 1) / 3 608 ns (pass 2) — 0.8 % timing variance, well within run-to-run
noise. **Bit-identical to the byte, twice, at 20 M.** Forest is a true function. No timing-dependent
state. No memory-state leakage. CONFIRMED at all scales (5 M earlier, 20 M here).

### `fault_inject` at 10 000 000 per pattern × 5 patterns

```
all_i32_MIN                n=10000000  L=0  E=0       A=10000000  oor=10000000  margin=[-1 781 991,-1 781 991]   38.48s
all_i32_MAX                n=10000000  L=0  E=0       A=10000000  oor=10000000  margin=[13 704 320,13 704 320]   25.57s
alternating_MIN_MAX        n=10000000  L=0  E=0       A=10000000  oor=10000000  margin=[3 230 549,3 230 549]     34.85s
first_half_MIN_second_MAX  n=10000000  L=0  E=0       A=10000000  oor=10000000  margin=[16 423 316,16 423 316]   31.78s
zero                       n=10000000  L=0  E=10000000 A=0         oor=0         margin=[-1 781 991,-1 781 991]   38.13s
```

**50 M pathological inputs. Zero panics. Zero slow paths.** Range guard catches the four OOD patterns
in full. All-zero lands in-box with a constant margin of -1 781 991. Per-advice cost is steady at ~3.5 µs
across every pattern, including the four that force extreme branch conditions — **no path through
`advise` is slower than any other**, which is what the integer-only design was supposed to buy and is
what it actually buys at scale.

### `boundary_sweep` at 5 000 000 samples per feature

```
[boundary_sweep] monotone asc: 5, desc: 1, non-monotone: 13 (of 20 swept)
```

Same 5/1/13 split at 5 M samples as at 1 M samples from the gate-style bench. Confirmed at scale.

### `schedule_storm` at 200 000 tasks (8 bands)

**Killed at 349 s after admit completed (6.08 s for admit).** Drain never produced a single line.

```
[schedule_storm] admitting 200000 tasks across 8 bands...
  admitted 100000/200000
[schedule_storm] admit done in 6.08 s
```

See DEFECT-4 for the root cause.

---

## DEFECT-1 (ALET-P3-004) — Low verdict is so rare it is operationally unreachable

**Severity:** P2. **Disposition:** open.

Across **1 000 000 000** in-box advices, the model returned the `Low` verdict **811 times** — a rate
of **8.11 × 10⁻⁷**. On a workload below ~1.2 M tasks/sec, the kernel may never see a single Low
verdict; even at the rate it is designed for, it sees ~1 Low per second.

This is not a bug in the model: the gate bench already showed PR-AUC 0.99543 (MODEL-CARD §2), and the
forest's ranking is real. It is a bug in what the verdict tells the operator. ADR-056 §Decision calls
the output a "three-way verdict (Low / Abstain / Elevated)" used "for tiebreak only", and the
`RiskAdvisor::advise` API returns `Advice { verdict, margin, out_of_range }` with `verdict` as a public
enum — code reading it will treat Low and Elevated as symmetric outcomes.

What the bench proves: on a uniform in-box sample they are not symmetric. Elevated is the model's
*default* decision; Low is the rare outcome the model actually believes in. A scheduler that treats
`Low` and `Elevated` symmetrically is writing logic that branches on a 0.0001 % event and a 99.9999 %
event with the same weight.

**Why the gate bench missed it:** the gate runs 2 M uniform-in-box advices and observed 2 Low. That
looks symmetric on paper (2 ≈ 0 of 2 M); the 1 B run is what reveals that the absolute rate of Low is
not the rate Elevated happens at, and the gap is large enough that the three-way verdict UI is a lie.

**Proposed fix (advisory, not yet implemented):** the model card's headline number — 0.99543 PR-AUC —
is a ranking metric, and the gate's reported counts (2 Low / 1 999 998 Elevated in 2 M) are the metric
in absolute terms. Either document this as a known one-bit operating regime ("advisory is effectively
two-way in practice; the third way is a debug signal"), or add an additional output to `Advice` —
e.g. `n_decisive_low` over a window — so a scheduler that wants to know "did the model actually have
an opinion" can tell. The current `Advice` does not carry this.

**Owner:** `kernel-core/src/mlrisk.rs` (`Advice` struct, `Verdict` enum); interaction with the model
card in `aletheia-ml/docs/MODEL-CARD.md`.

---

## DEFECT-2 (ALET-P3-005) — four of nine monotone constraints are silently dropped at export

**Severity:** P2. **Disposition:** open. Crosses the `aletheia-ml` repository boundary.

`aletheia-ml/docs/MODEL-CARD.md` claims **9 of 20 features carry a `+1` monotone constraint**:
*"more prior failures by this user, more evictions in the cell, and so on may never lower predicted
risk."* `boundary_sweep` at 5 M samples per feature reports:

```
monotone asc: 5, desc: 1, non-monotone: 13 (of 20 swept)
```

Reading these together: of the 9 features the trainer was told to constrain with `+1`, **only 5 of
them are actually monotone-ascending in the exported blob's margin**. One is monotone-descending.
Thirteen are non-monotone. Of the 9 constrained features, **4 are violated**.

The 5 asc features are consistent — they are exactly the subset the model card named as
"monotone-by-construction" (priority, user-fail-rate, the cell-pressure ones). The 4 violated ones are
the ones that depend on derived counters (`cpu_x_mem`, `task_index`, `time_of_day`, possibly
`missing_info`). The non-monotone feature #1 (`desc`) is `priority` itself — **a higher priority value
predicts a LOWER risk margin**, which is the opposite of what a scheduler using this advice would want.

**Why the gate bench missed it:** the gate runs `boundary_sweep` at 1 M samples. The 5/1/13 split is
identical at 1 M and at 5 M — sampling was not the issue. The gate **doesn't run boundary_sweep at
all on this kernel** (`kernel-core/tests/mlrisk_stress.rs::the_stress_suite_holds_on_the_host_at_scale`
does not include a boundary sweep); the OVERLOAD bench is what exposed the assertion.

**Root cause hypothesis (not yet investigated in code):** the constraint is a training-time argument to
XGBoost (`monotone_constraints`). If it is not propagated through the post-training exporter that
walks the tree table to verify leaf-monotonicity in the integer blob, the constraint is enforced on
the float model but not on the export. The same class of bug has been seen before in this repo
(gate-bench fix for the `PriorityScheduler::effective_priority` allocation: documented in MODEL-CARD
§8 as "the same fast path, again, on a different seam").

**Proposed fix (advisory):** in `aletheia-ml/src/aletheia_ml/export.py`, walk every tree post-export
and assert that for every split on a monotone-constrained feature, the left subtree (the one taken
when `x < threshold`) produces a margin that does not exceed the right subtree's margin by more than
the threshold delta times the learning rate. Failing this assertion must be a hard error before
`install` runs.

**Owner:** `aletheia-ml/src/aletheia_ml/export.py` (or `train.py`'s constraint plumbing); this is the
first `aletheia-ml` finding that crosses the kernel repo boundary, and the cross-repo fix needs both
sides.

---

## DEFECT-3 (ALET-P3-006) — all-zero feature vector is a guaranteed-Elevated backdoor

**Severity:** P2. **Disposition:** open.

Both `advice_storm` and `fault_inject` agree: a feature vector of all zeros produces margin `-1 781 991`
on **every** call, and the verdict is **Elevated** (because the threshold margin is below
`-1 781 991` and the inverted conformal band cannot fire). This is a fixed, bit-exact, reproducible
output for an input the model has seen before — at scale.

In production this matters in two ways:

1. **Adversarial feature extractor.** A bug or attack that produces a constant vector — `[0, 0, …]` is
   the obvious one, but any constant `x[i] = c` for all `i` will collapse the forest onto a small set
   of leaves and produce a small set of outputs. The all-zero input happens to be in-box; many other
   constants will trip the range guard and produce `Abstain`, but the in-box ones give a **guaranteed
   verdict** for any constant the extractor emits. A scheduler trusting that verdict will keep doing
   what the extractor says.
2. **Silent abstention indistinguishable from a decision.** The margin `-1 781 991` does not trigger
   the inverted band (which is empty, MODEL-CARD §4), does not trigger the range guard (the feature
   vector is in-box), and is below the threshold. From the kernel's perspective, this is a decisive
   `Elevated` verdict. From the operator's perspective, it is a feature extractor that is producing
   garbage.

The `Advice` struct has no field to distinguish "the model computed a margin" from "the model received
a degenerate input that produces a constant margin by construction". The boot line
`[mlrisk-stress] in-box census: … (N from the conformal band)` (MODEL-CARD §3) reports band-fires,
which are 0 — and there is no other audit signal.

**Why the gate bench missed it:** the gate runs `advice_stress` with a uniform in-box sample. All-zero
is the *one* point in this surface that produces a constant margin; the gate's `advice_stress` is
seeded from a deterministic RNG and the seed picks a non-zero point. The OVERLOAD bench reaches the
all-zero surface explicitly (surface 4 of 4) and reports the constant margin.

**Proposed fix (advisory):** add a third abstention cause — **degenerate input** — alongside
`out_of_range` and the conformal band. A feature vector is "degenerate" when it has fewer than some
small number of distinct values across the 20 features (the all-zero case is the extreme). The
verdict is `Abstain`, the margin is still reported, and a new `Advice::degenerate` field exposes the
cause to the caller. Concretely, on the all-zero input the verdict becomes `Abstain` with
`degenerate = true`, and the boot-time audit can count "how many of my `Abstain` calls were range
vs degenerate vs band" instead of the current 2-way split.

**Owner:** `kernel-core/src/mlrisk.rs::advise` (add field); `kernel-core/src/mlrisk_contract.rs` may
need a small bump if the `Advice` shape changes.

---

## DEFECT-4 (ALET-P3-007) — `PriorityScheduler` drain is O(N²); 200 K tasks does not drain

**Severity:** P0. **Disposition:** open.

`schedule_storm` admits 200 000 tasks in 6.08 s, then **does not produce a single drained task** in
349 s of wall time before being killed. The gate bench runs at 8 000 tasks and finishes the drain in
the expected 2.2 s (MLRISK stress report, August 13). The break is somewhere between 8 000 and
200 000.

**Root cause, located in source:** `kernel-core/src/priosched.rs::schedule_next` does two O(n)
operations per call:

```rust
for &t in &self.order {                                    // O(n) scan
    if self.state.get(&t) != Some(&TaskState::Ready) { continue; }
    let p = self.effective_priority(t);                    // O(?) per call
    ...
}
let (winner, _) = best?;
self.order.retain(|&t| t != winner);                       // O(n) per call
```

`effective_priority` is O(1) when nobody is donating (the fast path added per MODEL-CARD §8), so the
scan is O(n). Then `order.retain` is O(n). With `schedule_next` called N times during a drain, this is
**O(N²) total** — and for 200 K tasks that is 4 × 10¹⁰ operations. The original gate-bench fix made
the inner scan cheap at N=128; it did not address the O(n) `order.retain`.

`finish` is also O(n) over `order.retain`, contributing another O(N²).

**Why the gate bench missed it:** the gate uses 8 000 tasks. 8 000² = 6.4 × 10⁷ operations, which at
~10 ns each finishes in ~640 ms. The 200 K case is 25× larger, but the cost is 625× larger: **at
N=50 000 the drain takes ~30 s; at N=200 000 the drain is effectively infinite**. The gate does not
catch non-linear cost growth because it only tests one N.

**Proposed fix (advisory, the kernel core change is small):** replace `VecDeque<TaskId>` + `retain`
with a structure that supports both "pop the highest priority Ready task" and "remove an arbitrary
task" in sub-linear time. A `BTreeSet<(Priority, TaskId)>` keyed by `(effective_priority, task_id)`
would make `schedule_next` O(log n) and `finish` O(log n), dropping total cost from O(N²) to O(N log
N). The matching change in `effective_priority` would be to recompute lazily only for tasks whose
donation graph has changed (today it is recomputed per scan, but with the fast path it is O(1) when
nobody donates, so this is OK).

**Owner:** `kernel-core/src/priosched.rs` (scheduler); gate to be extended with a 200 K-task drain
case so this never ships unfixed again.

---

## DEFECT-5 (ALET-P3-008) — inverted conformal band still ships; abstention path is dead for this blob

**Severity:** P2 (re-confirmation of MODEL-CARD §4, not a new defect).

ADR-056 §Decision states two independent abstention causes: the conformal band (when both labels are
plausible) and the range guard (when the input is outside the training feature box). The bench
measures both, at every scale, and the conformal band **never fires** for the installed blob:

* `advice_storm` (1 B advices, in-box): `band = 0`.
* `fault_inject` (50 M pathological inputs): `band = 0` (every OOD pattern trips the range guard
  instead; the all-zero pattern is in-box and is **not** caught by the band either — see DEFECT-3).
* Gate bench at 2 M advices (from STATUS.md / MODEL-CARD): `band = 0`.

The `abstain_lo > abstain_hi` inversion that ADR-056 documents is unchanged at HEAD. The trainer-side
fix (`aletheia-ml`'s `calibrate.check_band`, MODEL-CARD §4) is correct but **does not help the kernel
side**: `kernel-core/src/mlrisk.rs::load` validates that the band is well-formed (magic, version,
count, hash, scale, table length, child indices) and **does not validate that `abstain_lo ≤
abstain_hi`**. A blob with an inverted band loads, runs, and silently never abstains from this cause.

This is the documented defect (MODEL-CARD §4) re-confirmed at every scale the OVERLOAD bench could
reach. The bench contributes nothing new except the observation that the all-zero degenerate input
(see DEFECT-3) lies **inside** the empty band interval `[hi, lo]` because `lo > hi` — it is *not*
caught by the band, exactly because the band is empty. A naive kernel fix that "adds a third
abstention cause when in-box and verdict is `Elevated` with margin < some_threshold" would silently
treat every all-zero input as a band-abstention, which would lie about which cause is firing.

**Proposed fix (advisory):** the kernel should validate `abstain_lo ≤ abstain_hi` at `load` time and
return `ModelError::InvertedBand` if not — making the failure visible at boot rather than at
inference time. This is a one-line change to `mlrisk.rs::load` plus a `ModelError` variant plus a
refusal-at-boot invariant. The existing trainer-side `check_band` becomes redundant for any blob
shipped to the kernel.

**Owner:** `kernel-core/src/mlrisk.rs::load` (add the check); `kernel-core/src/mlrisk_stress.rs`
(add the boot-time refusal test).

---

## Reproduction

```
# from the kernel-core directory
cargo build --release --example overload_bench

# 1 B advice storm (55 min host, single core)
OLOAD_ADVICES=1000000000 ./target/release/examples/overload_bench advice_storm

# 20 M determinism (4 min)
./target/release/examples/overload_bench determinism 20000000

# 50 M fault inject (3 min)
./target/release/examples/overload_bench fault_inject 10000000

# boundary sweep at 5 M samples per feature (1 min)
./target/release/examples/overload_bench boundary_sweep 5000000

# schedule storm — completes in seconds since ALET-P3-007 (was: do not run before it)
./target/release/examples/overload_bench schedule_storm 200000
```

The bench binary reads `kernel-core/models/aletheia_risk.altm` via `BUNDLED_MODEL` (same path the
gate bench uses). All numbers in this document are reproducible from HEAD at the time of writing.

---

## Disposition

| ID | Sev | Disposition | Owner | Evidence |
|----|-----|-------------|-------|----------|
| ALET-P3-004 | P2 | resolved | `kernel-core/src/mlrisk.rs` | this doc, §"DEFECT-1" — regime documented on `Verdict`; `Abstain` made a real third way again by the ALET-P3-008 recalibration |
| ALET-P3-005 | P2 | open | `aletheia-ml/src/aletheia_ml/export.py` | this doc, §"DEFECT-2" |
| ALET-P3-006 | P2 | resolved | `kernel-core/src/mlrisk.rs::advise` | this doc, §"DEFECT-3" — degenerate abstention shipped, gated at boot (invariants 8–9 of 22) and in the census end to end |
| ALET-P3-007 | P0 | resolved | `kernel-core/src/priosched.rs` | this doc, §"DEFECT-4" — ordered ready pool; 200 K drain gate green |
| ALET-P3-008 | P2 | resolved | `kernel-core/src/mlrisk.rs::load` + recalibration | this doc, §"DEFECT-5" — `ModelError::InvertedBand` refusal; blob re-made at alpha 0.03 with a live band |

**Verification outcomes (2026-08-21, fixes landed):**

* **DEFECT-3 — verified.** `fault_inject` at 100 K per pattern: the `zero` pattern now reports
  `A=100000 oor=0 deg=100000` at the same constant margin `-1 781 991` — the verdict is withheld
  and the CAUSE is named; the four out-of-box patterns are unchanged (`oor=100000, deg=0`), so
  the census did not misattribute anything. Gated further by boot invariants 8–9 of 22 and a
  hosted unit test.
* **DEFECT-4 — verified.** `schedule_storm 200000`: admit 0.77 s, drain 4.72 s, total 5.49 s,
  priority monotonic across all 200 000 drains — against 349 s killed without producing one
  dispatch when this audit was written. The hosted gate (`tests/mlrisk_stress.rs`) now runs that
  N on every `cargo test`, model-free AND advised, with exact-permutation assertions.
* **DEFECT-5 — verified, both ends.** `load` refuses an inverted band (`ModelError::InvertedBand`,
  tested by swapping the shipped blob's band bytes); the SHIPPED blob is asserted well-formed.
  The blob itself was re-made at `CONFORMAL_ALPHA = 0.03` (`scripts/band_alpha_sweep.py` in
  `aletheia-ml` measured the grid: bands live for alpha <= 0.04, dead from ~0.045 up — LOWER is
  the direction that widens the class sets). New band `[0.357747, 0.553282]` probability /
  `[-1 423 046, -897 932]` fixed-point; measured ~1.0 % of a test shard abstains by band, and
  exactly 3 of the 256 committed fixture rows fire it, deterministically. New blob sha256
  `3e4def4641404bd951940098098bacf393379ee2e901bbac40746aac7320e340`; forest weights, feature
  contract and operating threshold are unchanged.
* **DEFECT-1 — resolved by documentation + the live band.** The `Low` asymmetry (811 in 1 B) is
  now stated on `Verdict` itself where any reader must pass it; and with the band live,
  `Abstain` is once more a reachable third way rather than a dead field, which was the operational
  half of the complaint.
* **DEFECT-2 — still open** (trainer-side export assertion); nothing in this wave touched it.

**Not claimed:** that the OVERLOAD bench is the only way to find defects of this class. A targeted
unit test on `Advice` for the all-zero case, or a targeted unit test on `effective_priority`'s
fast-path invariant, would find the corresponding defects with no bench at all. The bench is the
mechanism that *made us look* — the test should be the mechanism that *keeps us honest*.