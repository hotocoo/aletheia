# ADR-077: Lethe — the resident performance advisor for the power/performance contract

**Status:** Accepted · **Date:** 2026-08-28 · **Advances:** REQ-ML-006 (new; the advisor rung of
the power/performance contract) · **Builds on:** ADR-076 (frequency is authority, heat is a hard
ceiling), ADR-056 (the ML risk advisor; advisory by construction), ADR-061 (marker maps),
ADR-063 (never-freeing heap discipline)

## Context

ADR-076's demand governor is memoryless: each step reads one demand sample and maps it onto the
governor range. On real workloads a memoryless governor pays for that twice — it chases every
dip of a bursty trace down and every burst back up, and ramp latency is real even when this
kernel's model does not charge for it; and it leaves the question of PARKING entirely to its
caller, so the cheapest legal idle state is only reached if a caller remembers to ask.

The ML answer to "is this task safe to admit" is `mlrisk` (ADR-056). The power contract faces a
different question every tick: *where should this domain's clock be in a few ticks, and is this
pause worth waking from?* That is also a tabular-prediction question, not a language question —
so it gets the same instrument: a frozen integer model, trained outside the kernel, embedded in
every image, verified at boot, and ADVISORY in exactly the ADR-056 sense. The user-facing name
for the machine's intelligence is Lethe; this wave makes a piece of it resident in the
power/performance path.

## Decision

### Two frozen decision trees, packed into an `ALTH1` blob

`kernel-core/src/lethe.rs` verifies and evaluates `models/lethe_pm.alth` — two decision trees
compiled to a flat table of `i32` compares (the ALTM discipline, new magic `ALTH`, version 1):

* **FREQ** — `Coast` (settle toward the lower third of the governor range), `Hold` (exactly the
  ADR-076 demand map), or `Boost` (pin the TOP of the governor range). Boost's meaning is
  deliberate: holding the top ahead of the demand register is what burst churn cannot take back
  — a fractional floor would degenerate Boost into the demand map on a 3-rung governor, which
  is Hold's job.
* **IDLE** — for a zero-demand domain: `Stay` awake at the lowest point, `Shallow` (C1), or
  `Deep` (C2).

The input is a 12-feature vector (`lethe_contract.rs`: demand history statistics over 16
samples, dwell and churn at the current point, reported temperature against the trip margin,
and the current point's share of the governor range). The contract carries a sha256 identity
hash, so a blob whose feature *meanings* have moved is a named refusal, never a silently
rotated set of columns. Every way a blob can be wrong is a named [`LoadError`]: short, bad
magic, wrong version, wrong feature count, contract mismatch, truncation, empty forest, bad
index (including a CYCLE — load walks each tree with a visited set, because an evaluate-time
loop would be a hang, not an error), bad class, inverted box (the range guard must be able to
fire), and a box outside the contract's domain.

### Advisory by construction, restated for the power path

The advisor proposes; the power/performance contract disposes — through the SAME named APIs any
caller uses (`request_index`, `wake`, `enter_idle`):

* With Lethe present, the overclock band stays authority-only and the envelope absolute: every
  target the advised path can pick is a min over indices at or below nominal, and `request_index`
  enforces the same contract for the advised path as for any other caller. Proved at boot, with
  a full-ceiling grant minted, over multi-regime traces.
* Demanded silicon is never parked: the advised path wakes first and serves; `enter_idle` is
  only ever called on a zero-demand, awake domain — so `DomainBusy` can never even fire.
* With the advisor ABSENT, or abstaining (out of the training box, or a degenerate all-equal
  input), the advised path performs exactly the ADR-076 demand map and parks nothing: the clock
  sequence is bit-identical to the baseline governor, step for step. Absence and abstention are
  counted in the report, never silent.
* Device power is untouched: the advised path never calls `set_device_power`; the arcs move
  only through the contract's own API.

### The trainer, the corpus, and the honest comparison

`docs/evidence/lethe006/lethe_train.py` (vendored, deterministic, seeds fixed, stdlib only)
owns the corpus, the fitting, and the export:

* **Corpus:** 300 train + 300 held-out traces of 256 steps across six documented regimes —
  idle, steady, bursty, ramp, staccato (interactive/GUI-style fast alternation), sawtooth —
  with a thermal stand-in that heats on the clock each arm ACTUALLY ran at (temperature is the
  caller's duty under ADR-076; the simulator is a training stand-in, not a claim about
  silicon).
* **Fitting:** cost-sensitive depth-3 CART on EXPECTED per-row cost (sums alone let a class's
  rare disasters veto it everywhere), labels from K=16-step class-consistent counterfactual
  rollouts (a one-step-then-baseline label cannot see that Boost's value accrues AFTER the
  step that pays for it), collected twice DAgger-style on the fitted policy's own trajectory
  so features describe the states the advisor itself visits.
* **The comparison (the benchmark proof this wave ships):** the same held-out traces driven
  through six arms under a documented cost model (ramp latency R=2 steps, wake penalties of 1
  step (C1) and 3 (C2), CV² energy with the ladder's own mV, parked energy at 5% of the lowest
  awake point), with unmet work weighted 10× energy (α swept at {1, 3, 10} in the results).
  Arms: the ADR-076 baseline, an eager C2 parker, a TUNED classic hysteresis (the honest
  competitor — given its own tuning budget), Lethe, always-nominal, always-low.

**Result on held-out traces (score = α·unmet-work + energy, lower is better):** Lethe 0.6100
beats the baseline 0.6280 (+2.88%) and the tuned hysteresis 0.6876 (+11.29%), and dominates the
baseline on BOTH components (unmet work 0.0158 vs 0.0174; energy 0.4516 vs 0.4537). The
decomposition is in `results.json` and it is not uniformly flattering: Lethe's lead is the
IDLE policy (idle regimes 0.017 vs 0.097 — parking when pauses are long beats never parking)
plus Boost's anti-churn pinning on staccato traces (1.017 vs 1.083), while it LOSES the bursty
regime to the baseline (0.869 vs 0.857). `results.json` also carries the ramp-latency
sensitivity (R ∈ {0,1,2,4}) and the α sweep.

### Parity is a committed fixture, replayed through the live observer

The trainer emits `models/lethe_pm_fixture.tsv` (16 rows): each row is a self-contained
observation stream (starting exactly at the last position change, so a fresh observer's forced
first flag agrees with the trainer's history) plus the features and both advice classes. Every
boot replays the whole fixture through `PmObserver` and requires features AND classes to match
the trainer exactly. Feature derivation is EXCLUSIVE (history strictly before the advice being
advised on) and clamped into the contract's domain.

### Proof posture

Host-exhaustive (`kernel-core/tests/lethe.rs`, 13 tests): the full mutation table for every
named refusal (including the cycle check); contract/blob agreement; fixture parity with
determinism; the absent-advisor and abstaining-advisor equivalence sweeps over randomized
multi-regime traces (bit-identical state sequences against a paired baseline engine); the
safety sweep (grants minted, every applied point ≤ nominal, parks only at zero demand,
residency monotone, wake latency a sum of real wake costs, `pm_refusals == 0` by construction);
engine-level determinism including the audit ledger; ledger monotonicity across wraparound;
observer bounds with features in-domain for any randomized stream; degenerate-input
withholding; a REPORTED (never gated) advice-cost measurement; and the boot suite itself
running on the host.

In-kernel: `lethe_suite`, 12 invariants on every boot of all three targets
(`[lethe] ALL 12 LETHE ADVISOR INVARIANTS HOLD`, boot fails 580+i). Seven are pinned cross-CPU
in the conformance contract: the blob verifies, wrong blobs are refused by name, the fixture
replays exactly, the governor range is never left, demanded silicon is never parked, the
absent-advisor path is bit-identical to the baseline, and the observer is bounded.

### Why the ML goes in as a frozen table and not as the model

The same boundary ADR-053 drew for the console: the model does not go into the kernel. A frozen
integer table is verifiable, deterministic, allocation-free after load, and safe to consult on
the hot path; an inference runtime in kernel space would be none of those things. When the
hosted Lethe runtime wants to influence clocks, the correct door is the one this wave built:
a verified `ALTH1` blob is DATA, and data crosses the boundary the same way the trained
scheduling forest did.

## Consequences

* **Named non-claims.** The benefit numbers live in the trainer's documented cost model — this
  kernel still models transitions as free, and a hardware rung (MSR/CPPC, ADR-076's register)
  would measure the real ones. No live governor thread exists yet, so "resident" means "wired
  into the model's govern path and proved at boot" — the same posture `mlsched` had before its
  resident wave. No battery, no S3, no voltage enforcement. The simulator's thermal stand-in is
  a training/evaluation artifact, not a thermodynamic model. And the corpus is SYNTHETIC: the
  comparison says the advisor helps under the documented cost model on these regimes — it does
  not say Lethe beats Linux, Windows, or any real OS at anything (the ADR-056 honesty rule).
* **The tuned hysteresis is published alongside the model.** A simple rule with a tuning budget
  is the competitor most "AI" features quietly skip; here it is in the results table, it loses
  to Lethe by 11.29%, and the per-regime table shows where.
* **Marker map changed deliberately** (`lethe=12` on the aarch64, RISC-V and x86-64 gates;
  the VirtualBox gate requires the marker; ADR-061), and the conformance contract grew seven
  Lethe behaviors on all three targets.
* **`PmEngine::govern` is byte-for-byte unchanged.** The advised path is an additive layer
  (`govern_advised` + read-only accessors + `request_index`, which shares `request_point`'s
  exact internals); every pre-existing pm proof still runs against the untouched baseline.
