# ADR-237 — System 1 native in the OS: audit and gap register

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-AI-017 (new, the register this ADR keeps)
**Builds on:** ADR-056 (advice reorders equals only), ADR-081/082 (memory boundary, reclaim), ADR-165/166 (Lethe, resident governor), ADR-186 (System 1/System 2 as registry roles), ADR-199 (scheduler advised during life), ADR-212/213 (programs run together, background jobs), ADR-231 (scheduler checkpoint), ADR-232 (block cache).

## Context

The goal for the next waves is a System 1 built into the OS for every latency-critical path
(scheduling, memory, caching, I/O, power, anomaly detection, prioritization), System 2 kept as it
is, and Laya shipped as a first-class Aletheia capability rather than a manual sidecar. Before any
of that is built, this ADR records what exists, verified against the tree at `88661be`, so each
later wave closes a named row instead of a vague ambition.

"System 1" in this ADR means two different things, and the register keeps them apart:

* **In-kernel System 1:** frozen integer models (`ALTM1`/`ALTH1` flat tables of `i32`
  compares, no floating point, no allocation after load, verified at boot) consulted on a
  latency path, with an abstention that is bit-identical to the model-free kernel.
* **Decision-wire System 1:** a typed-decision model (Laya and its fine-tunes) answering
  `POST /v1/decide` for the host daemon `aletheiad` (ADR-186), escalating to System 2.

A 421 M-parameter encoder does not belong on a dispatch path that runs in microseconds; a
frozen forest does not answer a console question. Both are System 1, at different time scales.

## Audit: what exists (verified)

| Path | System 1 today | Where | Live? |
|---|---|---|---|
| Scheduler admission + equal-priority order | `mlrisk` forest (20 features, `taskfeat`) | `mlsched::resident::admit` | Yes: `run_programs` (console `run`/`together`), `run_advised_scheduler` (boot + console `tasks`) on all three CPUs |
| Background programs (`start`, ADR-213) | none | `jobs::Jobs::next_turn` is plain round-robin | Turns are not ordered by priority or advice; only outcomes are fed back (`observe_outcome`) |
| Memory admission | allocator meter (hard bound, not a model) | `resident::observe_memory` before each admission | Yes |
| Reclaim under pressure | `memrisk` forest as tier order | `reclaim::Reclaimer` | **No.** Boot suite and storm only; a running machine's pressure consults nothing |
| Power/performance | Lethe forest (`ALTH1`) as governor | `lethed::resident::on_timer_tick` | Yes, off the timer on all three CPUs |
| Block cache | none (CLOCK, 16 blocks) | `bcache::BlockCache` | Cache live (ADR-232); no prediction, no prefetch |
| Anomaly detection | none in kernel | supervisor counts faults (ADR-202), fuzz gates offline | No runtime detector |
| I/O scheduling | none | drivers are polled, one request in flight | No queue to order |
| Console decisions | Laya fine-tune `aletheia-console-s1` (v2) | `aletheiad`, `ai/dual.rs` | Yes, when the operator starts the sidecar by hand |
| Scheduler decision over the wire | ADR-231 checkpoint, 99.5 % on unseen seed | benchmark only | Not shipped, not in any path |

Laya as shipped: `models/laya.toml` (base, `unfit`) and `models/aletheia-console-s1.toml`
(`ready`, default System 1, pulled as a digest-checked release asset). Running it needs Python,
`pip install laya`, and a person to start `scripts/system1/laya_server.py`. Nothing supervises
it, and a stopped sidecar is noticed only when a decision times out.

## Decisions

1. **The role stays model-agnostic; the backend becomes first-class.** ADR-186 said no Laya names
   in Rust and no hardcoding of the System-1 occupant. That stands for the *role*: the registry
   selects whatever manifest holds `system1`. What changes is the *backend*: the `laya` backend
   is shipped and supervised by Aletheia (started, health-checked, restarted, stopped with
   `aletheiad`), its checkpoint is provisioned from the release, and its absence is reported, not
   discovered by a timeout. That is a backend decision, recorded in its own ADR when built.
2. **Every in-kernel System 1 follows ADR-056's shape:** advisory ordering under a deterministic
   policy, abstention bit-identical to no model, a bounded worst-case cost measured at boot, zero
   bytes allocated per decision (storm discipline), and a counter that says how often it spoke.
   A model never decides *whether* something happens (admission, eviction need, frequency
   ceiling); it orders *which* among legal choices.
3. **External code is allowed where building it from scratch is not credible** (operator
   direction, 2026-10-08), graphics drivers named explicitly. Any such import is pinned by
   version and digest, licensed compatibly, and recorded in `third_party/` and an ADR.
4. **Waves close rows in this order**, each with its own measurement:
   1. Reclaim resident: a live machine's pressure opens a reclaim round over the programs it is
      running (background jobs first), ranked by `memrisk`; host proof plus a live call site on
      all three CPUs.
   2. Background programs ordered by priority with advised ties instead of blind round-robin.
   3. Laya backend supervised and provisioned by `aletheiad`.
   4. Block cache: measure the hit rate of the namespace workload first; only then a learned or
      adaptive policy, and only if it beats CLOCK on that number.
   5. Runtime anomaly detection over counters the kernel already keeps (faults, refusals,
      pressure entries, preemptions).

## Open rows this ADR does not pretend to close

These are named so the register does not drift into implying them:

* **GPU 3D, native 3A-class gaming, virtualization (Aletheia as a hypervisor):** not started.
  The platform path that exists is virtio-gpu 2D. QEMU offers virgl/venus for 3D; that would be
  the first rung, likely through imported driver code under decision 3.
* **Real-hardware overclocking:** the contract and the grant-only band exist (ADR-076, ADR-184);
  QEMU has no HWP actuator, so no frequency or voltage was ever changed on silicon.
* **Interrupt-driven I/O, multi-queue, more than one request in flight:** every driver polls.
  An I/O System 1 has nothing to order until there is a queue.
* **User authentication:** the console is a privileged root policy (MATURITY row).
* **Competing-OS comparisons** exist for boot, idle CPU, echo and IPC (docs/BENCHMARKS.md);
  graphics, gaming and sustained-load comparisons do not.

`docs/evidence/maturity-diagnosis-2026-09-30.md` (an earlier session's LLM-written review,
committed here as found) reaches the same headline gaps.

## Consequences

* Each later wave cites the row it closes and updates this table's "Live?" column in its own ADR.
* REQ-AI-017 is `partial` until rows 1, 2, 3 and 5 of decision 4 are live.
