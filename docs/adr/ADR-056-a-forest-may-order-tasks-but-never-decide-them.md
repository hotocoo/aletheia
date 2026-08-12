# ADR-056 — A forest may order tasks, but never decide them

**Status:** Accepted
**Date:** 2026-08-10
**Supersedes:** nothing. **Related:** ADR-006 (the intelligence runtime is the only probabilistic
stage), ADR-020 (priority inheritance), ADR-052 (the model is a system property), INV-014.

## Context

Aletheia's scheduler answers the same question millions of times an hour: *is this task going to be
evicted, killed, or fail if I admit it now?* Answering it well is worth real capacity — a doomed task
costs its cell an allocation plus the retry — and the trace evidence says it is predictable from
information available at submit time.

Three routes were considered.

1. **Ask the intelligence runtime.** Wrong instrument at any price. It is four orders of magnitude
   too slow for a scheduler hot path (10² ms versus the 10⁰ µs budget), it needs floating point that
   `kernel-core` does not have — `grep -c 'f32\|f64' kernel-core/src` is **0**, and the bare-metal
   targets are built without an FP ABI — and it is not reproducible run to run. It would also make a
   correctness-adjacent path depend on a service that may be absent.
2. **Hand-write a heuristic.** This is what schedulers usually do, and it is what the deterministic
   pipeline already does elsewhere. It is honest but leaves the measurable signal on the floor, and
   every constant in it is an unfalsifiable opinion.
3. **A frozen, verified, integer-only decision forest, advisory only.** Chosen.

The 2026 state of the art in tabular ML is split: in-context tabular foundation models lead the
public leaderboards on small-to-medium data, while gradient boosting still leads on large numeric
data — and only boosting can be compiled into something a kernel can evaluate. The reasoning, with
citations and the numbers that decide it, is in the `aletheia-ml` repository under
`docs/RESEARCH-SOTA-ML-2026.md`. The precedent for trees-in-a-kernel is Linux's: eBPF forbids
floating point outright, so in-kernel ML there is decision trees in integers.

The hard part is not accuracy. It is that `aletheia/src/intelligence.rs` opens by declaring the
intelligence runtime *the only probabilistic stage*, whose output "flows through the identical
downstream pipeline … never a bypass (INV-014)". A forest in the kernel is a second probabilistic
thing. Either that sentence becomes false, or the forest is introduced under constraints that keep it
true.

## Decision

`kernel-core/src/mlrisk.rs` carries a frozen risk forest under five constraints, and the module's
shape — not a convention — enforces them.

1. **It produces an ordering hint, and nothing else.** No plan, no action, no capability, no
   admission verdict. `RiskAdvisor::advise` returns `Advice { verdict, margin, out_of_range }`; there
   is no API by which it can emit anything a downstream stage would execute.
2. **It never gates correctness.** Every invariant, capability check, and admission rule holds
   identically whether the model is loaded, absent, or wrong. The only thing it may change is the
   order of tasks whose *effective priority is already equal* — a tiebreak that was previously
   FIFO-by-age and is unspecified policy. Priority is never traded for risk:
   `decisive_advice_reorders_only_within_equal_priority` asserts a higher-priority high-risk task
   still runs before a lower-priority low-risk one.
3. **With no model loaded, behaviour is bit-identical to the model-free kernel.**
   `advice_absent_matches_model_free_order` asserts this rather than assuming it. An `Abstain`
   verdict is stored as *no verdict*, so abstention is genuinely no opinion rather than a middle
   opinion, and a scheduler with an abstaining model schedules exactly as one with no model.
4. **It abstains rather than guesses.** Two independent causes, both integer-cheap: inside the
   class-conditional (Mondrian) conformal band, both labels are plausible; outside the per-feature
   box seen in training, the input is a question the blob was never asked. Either way the kernel
   declines and its deterministic policy stands.

   **Two causes means two failure modes, and one of them has occurred.** The blob installed in
   2026-08 (`borg2019`) shipped an **inverted** conformal band — `lo` above `hi`, an empty interval —
   so that half of this clause is dead for that model: measured band-abstain rate 0.000. Nothing in
   the kernel catches it, and deliberately so: `load` validates what makes a blob *evaluable*, and an
   empty interval evaluates perfectly well. The catch belongs in the trainer, at the moment the band
   is computed, and now lives there as a named refusal (`aletheia-ml`, `calibrate.check_band`). The
   range guard is unaffected and is the cause that fires in practice — 43 % of in-corpus rows, 98.4 %
   of rows drawn from a corpus eight years older. Anyone reading this clause as a live safety
   guarantee should check which of the two causes the installed blob actually has:
   `aletheia-ml/docs/MODEL-CARD.md` states it, and the boot line
   `[mlrisk-stress] in-box census: … (N from the conformal band)` measures it on the running machine.
5. **Absence and corruption are named, never silent.** `RiskAdvisor::load` returns a specific
   `ModelError` for wrong magic, unsupported version, feature-count mismatch, feature-contract-hash
   mismatch, wrong fixed-point scale, an empty forest, a truncated or over-long table, and any child
   or root index that is out of range or points backwards. This follows the precedent already set by
   `models/aletheia-lm.toml`, which exists before its weights do so that selecting it produces a
   refusal by name instead of quietly serving something else. `every_malformed_blob_is_a_named_refusal`
   covers each variant.

Supporting decisions:

* **No in-kernel learning.** The kernel gets a frozen blob; adaptation happens offline and a new blob
  is a reviewable artifact with a hash. The reason is a security property, not a performance one: a
  model that trains itself inside the kernel is a model whose weights an unprivileged workload can
  steer by shaping its own behaviour, and the thing it steers is the scheduler that judges it.
* **The feature contract is compiled in.** `mlrisk_contract.rs` is generated by
  `python -m aletheia_ml install` and holds the feature count, order, scales, and a sha256 of the
  contract. A blob whose feature *meanings* moved while its shape stayed the same is exactly the
  failure a length check cannot catch, so it is a hash check.
* **Parity is a test, not a claim.** The exporter quantises features *before* training, so each split
  threshold ceiled to an integer reproduces the trainer's float comparison exactly for every input
  the runtime can produce. A committed fixture carries the trainer's own integer margins and
  verdicts; `margins_match_the_trainer_exactly` requires exact equality in Rust.
* **The cost bound is measured, not asserted.** `worst_case_compares()` walks the shipped table, so a
  scheduler agreeing to call this on a hot path is agreeing to a number it can check rather than to a
  training parameter it has to trust.

## Consequences

**Good.** The scheduler gains a measured prior at a cost of a few hundred integer compares, with no
new failure mode: the worst case for a wrong model is a slightly worse ordering among tasks that were
already interchangeable. INV-014 stays literally true — the intelligence runtime remains the only
probabilistic stage that can produce a *plan*. Every existing `kernel-core` test still passes
unchanged, which is the operational form of constraint 2.

**Bad.** There is now a data artifact in the kernel tree whose provenance lives in another
repository, and a generated source file that must be regenerated when the feature contract moves. A
stale pair fails loudly (contract hash, then parity fixture) rather than silently, which is the best
available trade but still a trade.

**Ugly.** The risk-aware tiebreak predicate is deliberately *not* a total order — it compares only
when both tasks carry decisive verdicts — so selection depends on the FIFO scan order among mixed
decisive/abstaining sets. That is deterministic and documented, but it is not a lattice, and anyone
extending it should keep the "abstain means no opinion" property rather than tidying it into a rank.

## Alternatives rejected

* **Logistic regression instead of a forest.** Would be smaller and trivially integerisable. Measured
  on the same split it scores PR-AUC 0.482 against the forest's 0.667 at a base rate of 0.265 — it is
  barely better than the base rate. Recorded in the sweep table rather than argued.
* **A tabular foundation model.** Leads the 2026 leaderboards on small-to-medium data, and cannot be
  evaluated in `no_std` with no floating point at any accuracy: in-context learning needs the
  training set resident and a transformer forward pass.
* **Let the model change priorities, not just order.** Rejected: that is the version where a wrong
  model starves a task, and where "advisory" stops being true.
* **Load the blob from the filesystem at boot instead of `include_bytes!`.** Deferred, not rejected.
  It needs a capability-scoped read plus a signature check to be worth doing, and the verification
  path above is the prerequisite for either.
