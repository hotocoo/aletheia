# Aletheia — Implementation Status

**As of:** 2026-09-12 (THE BOOT CLOCK IS SPLIT — the one column Aletheia lost carried a caveat in prose (it boots through OVMF; the Linux leg is `-kernel`-loaded and skips firmware), and a caveat that excuses a loss without measuring it is worth nothing, so it is now MEASURED: `boot_and_measure` timestamps `calling ExitBootServices` as well as the prompt, yielding firmware and kernel shares medianed over the same runs. OVMF costs ~1429 ms; ALETHEIA'S OWN KERNEL reaches an interactive prompt in ~1082 ms against Linux's ~1786 ms, about 1.65x faster — while STILL losing the total by ~0.72 s, because splitting a number does not win it and a machine booting through firmware takes longer than one handed the CPU. NON-CLAIM: the kernel shares are far closer to like-for-like than the totals but not identical work — Linux's includes loading and decompressing a 14.16 MB payload, Aletheia's starts from a 1.44 MB image firmware already placed; and NO boot path was optimized, the same binary was measured more carefully — REQ-PERF-001, ADR-082); before that: 2026-09-12 (A PARKED MACHINE COSTS NOTHING — the comparative benchmark said Aletheia LOST the idle column at 0.5% host CPU against Linux; two defects behind it, instrument then kernel: `boot_and_measure` polled with `sleep 1`, putting one SECOND of quantization on a 2-3 s measurement (now 5 ms; same binaries measure 2487 vs 1773 ms where the coarse poll said 3065 vs 2044, gap ~1.02 s -> ~0.71 s, pre-fix numbers RETIRED not reconciled), and the Linux leg needed Docker so the comparison SKIPped on daemon-less hosts (now builds the same busybox initramfs from Alpine's minirootfs with host tooling); then the kernel — masking IRQ0 at the 8259A stops DELIVERY but not the 8254 COUNTING, so the emulator modelled a device ticking 100x/s for a sleeping guest, and `pit::quiesce()` reprograms channel 0 to mode 0 (no reload) so the counter runs down once and STOPS. Idle 0.5% -> 0.0% across three runs against Linux 0.6%: four of five columns now to Aletheia (idle, payload 9.8x, typed round-trip ~9.5x, privileged LOC ~950x), boot to prompt STILL LOST by ~0.71 s and not excused — Aletheia boots through OVMF while the Linux leg is `-kernel`-loaded, and splitting that total into firmware and kernel shares is NOT done. NOTHING here measures security; NO other OS is measured — REQ-PERF-001, ADR-081); before that: 2026-09-12 (THE WATCH IS WIRED TO THE CLOCK — the resident governor runs on each target's REAL timer interrupt: x86-64's IRQ0 and the aarch64/RISC-V timer traps drive `lethed::resident`, one governor behind one lock for the machine's whole uptime, entered only through the new `SpinLock::try_lock` because a handler that spun for a lock held by the code it interrupted would deadlock the core — a miss is a COUNTED stand-down, reported never gated; an uncommissioned watch is a no-op so the interrupt may be wired first, and a second commissioning is refused; demand MEASURED from the machine's own busy/idle split (halted core -> 0% measured, governor at lowest point; working core -> 100% measured, governor at nominal and no further, nobody supplying either number); temperature a NAMED stand-in on every target; gated are the contract properties never the numbers (census balances, zero contract refusals, governor range never left, measured demand answered, advisor consulted live on x86-64 — boot fails 614-619); 15 boot invariants on all three targets (lethed=15, VirtualBox gate too); NOT YET: live consultation is x86-64 ONLY because aarch64/RISC-V arm their timer only for the ring-3 run and see six slices, under one 16-sample window, and say so; still no MSR/CPPC/ACPI programming; still NOTHING claimed about other operating systems — REQ-PM-002, ADR-080); before that: 2026-09-12 (THE ADVISOR TAKES THE WATCH — Lethe is RESIDENT: `kernel-core/src/lethed.rs` runs on the clock, services exactly one domain per tick round-robin with constant allocation-free work, MEASURES demand from busy/idle accounting over disjoint renormalizing windows, and treats the TICK as an authority question — a replayed, rolled-back, too-eager, nested or unattached tick is a named refusal that moves no state and lands in a census that balances at every instant; a window past the staleness ceiling is RESYNCED and the advisor withheld until a full 16-sample window of post-gap truth refills rather than guessed through; a latched thermal cooldown OUTRANKS the advisor for its whole duration; the resident holds no grant so the overclock band is unreachable by construction; every act flows through ADR-078's lifted sweep body so the advisor-absent path stays bit-identical to the ADR-076 baseline — 14 boot invariants on all three targets (lethed=14), seven pinned cross-CPU (159 -> 166), 15 host proofs; NOT YET: nothing calls `tick` from a real timer IRQ and no MSR/CPPC/ACPI programming exists — REQ-PM-002, ADR-079); before that: 2026-08-28 (LETHE — the resident performance advisor — advises the
power/performance contract: `kernel-core/src/lethe.rs` verifies a frozen integer model (two
decision trees in one `ALTH1` blob, a 12-feature contract hash making moved feature meanings a
named refusal) and its advised governor path consults it once per domain per step — FREQ advice
(Coast/Hold/Boost; Boost pins the top of the governor range) from demand history, churn, dwell
and thermal margin, IDLE advice (Stay/Shallow/Deep) for zero-demand domains; ADVISORY by
construction: with the advisor absent or abstaining the advised path is bit-identical to the
ADR-076 baseline governor, with it present the overclock band stays authority-only and demanded
silicon is never parked; parity with the trainer is a committed fixture replayed through the
live observer at every boot — 12 boot invariants on all three targets, seven pinned cross-CPU,
13 host proofs, and a vendored comparative benchmark where Lethe beats the ADR-076 baseline by
2.88% and a TUNED classic hysteresis by 11.29% under the documented cost model, losing the
bursty regime and saying so — REQ-ML-006, ADR-078); before that: 2026-08-28 (THE
COMPOSITION CONTRACT is modeled, not assumed — pixels are AUTHORITY and the scanout is a HARD
BOUND: surfaces minted with possession-based owner tokens gating every op, placements clipped
exactly to the scanout, the painter's order the owner-controlled z-order, size-honest fills,
same-frame screen-space damage, zero writes on unchanged frames — 14 boot invariants on all
three targets, six pinned cross-CPU, 11 host proofs, ADR-077); before that: 2026-08-28 (THE
POWER/PERFORMANCE CONTRACT is modeled, not assumed — frequency is
AUTHORITY and heat is a HARD CEILING: `kernel-core/src/pm.rs` gives every domain an honest
discrete ladder, keeps the governor range free to any caller, gates the overclock band behind
live per-domain elevation grants that attenuate on delegation and clamp the domain back to
nominal the moment their grant dies, makes the thermal envelope absolute BY CONSTRUCTION,
answers a thermal trip with a machine-wide clamp and a tick-exact cooldown that refuses even
valid grants, never lets the governor overclock or park demanded silicon, accounts idle
residency and wake latency exactly, moves device power along legal arcs only, and audits every
act in a bounded monotonic ledger — 14 boot invariants on all three targets, six behaviors
pinned cross-CPU, 19 host proofs, ADR-076); before that: 2026-08-26 (PER-DEVICE DMA WINDOWS are enforced by the real VT-d unit - each driven function
translates ONLY the frames its own driver registry granted; a revoked PAGE is denied by name with measured
reason 6 while sibling windows keep serving - ADR-075); before that: the IOMMU contract crosses the ARM fence — on x86-64 the kernel
discovers the VT-d unit through ACPI DMAR/DRHD, programs an identity domain over owned frames with
the kernel image punched out of it, adopts that domain via SRTP and turns enforcement ON, then
proves live enforcement from the unit's own fault bank: the granted function walks clean, a
revoked function is denied with an ACTIVE record naming its source-id and reason CONTEXT_ENTRY_P,
a restored grant returns to silence, and enforcement stays latched until halt — ADR-073); before that: the custody anchor crosses the platform boundary — the vault root is DELIVERED over the firmware configuration channel on all three targets (QEMU fw_cfg: ioports under q35+OVMF, MMIO on both virt machines), through one door that names every impostor — absent, firmware-absent, wrong-size, foreign-root, rolled-back — with a THIRD rootless boot in each gate proving absence seals the vault while the machine continues, and the combined-transaction question DECIDED: paired commits write the vault generation into the durable entity-store record so even a consistent older VAULT-pair rollback is caught BY NAME (ADR-072); before that: authority custody is a LIFECYCLE, not a caller-supplied key — the persisted registry gains `capvault`: a versioned data-key keystore sealed with in-tree RFC 8439 ChaCha20-Poly1305 under a root-derived subkey the vault alone retains, one-way rotation whose retirement DESTROYS the retired key, constructed prefix||counter nonces reserved before use because the kernel has no boot entropy, a three-commit rekey pivot crash-proved at EVERY recorded device-op position, and 17 custody invariants on every boot of all three targets — the custody half of ALET-P1-034 over authority, ADR-070; before that: encryption at rest is a LIFECYCLE, not a key file — the hosted semantic store gains versioned data keys under a root-derived keystore with rotation/rekey/retirement, constructed prefix||counter nonces whose ledger is the authenticated log itself, position-bound AEAD frames that refuse reordering/deletion/duplication with the position named, plaintext-SHA-256 identity semantics proved in both directions, and transparent wholesale migration of pre-ADR-069 logs detected by trial-authentication — closing the P1-028/029/030 trio over the store, ADR-069; before that: the supply chain is VERIFIED, LIVE, and RECORDED — chain verification crosses the installation boundary: root→signing-key→component provenance is enforced at install against public keys only, admitted entities record their full evidence, the launch gate re-judges that evidence against CURRENT trust so signer revocation goes live at the next launch, all faults are named per link, and the spawn path — found skipping provenance entirely, ALET-P2-050 — now passes the same gate, ADR-067; before that: the component DECLARES what it speaks — the ABI is explicitly versioned: a custom-section declaration enforced at BOTH gates, install refusing undeclared/malformed/foreign-version modules before their bytes are stored and run re-checking on every path, refusals naming both sides of a version disagreement, in... (line truncated to 2000 chars)
**Milestone delivered:** M1 — Hosted System-Core Reference (Rust); **P2 (start)** — WASM capability-secure component runtime; **P4 (start)** — bootable microkernel on THREE CPU targets, VM-tested: aarch64 (bootstrap) + AMD64/x86-64 (first-class) + **RISC-V/RV64GC (first-class)**; **P5 (start)** — real memory management: physical page-frame allocator + MMU virtual memory (identity map + dynamic map/unmap) + **EL0 user-mode with a capability-gated syscall boundary, hardware address-space isolation, per-process address spaces (separate TTBR0), and preemptive multitasking (full trap-frame context switch + round-robin scheduler + GICv2/generic-timer IRQ preemption)**, VM-tested on the aarch64 dev backend
**Maturity:** `docs/MATURITY.md` grades every subsystem Proved / Implemented / Architecture and states
plainly that **nothing here is production-ready** — read it before quoting any claim below.
**Sources of truth:** `docs/Aletheia_Product_Requirements_Document.md` (PRD-003),
`docs/Aletheia_Software_Architecture_Document.md` (SAD-002), `docs/adr/ADR-001..078`.

## Current wave — the boot clock is split (2026-09-12, ADR-082)

ADR-081 left one column lost and refused to spend the caveat that might have excused it: *splitting
Aletheia's total into a firmware share and a kernel share has not been done, so no part of that gap
is currently excused.* A caveat that excuses a loss without measuring it is worth nothing. Either
the firmware share is real and can be shown, or the caveat should be deleted.

`boot_and_measure` now takes an optional `SPLIT_MARKER` — the line a guest prints the moment it owns
the machine, which for Aletheia is `calling ExitBootServices`. One boot yields two numbers, medianed
over the same runs as everything else. The marker is cleared before the Linux leg, because Linux has
no firmware share here: `-kernel` loading is the start of its own work, so its total IS its kernel
share.

| | Aletheia | Linux 6.12-lts |
|---|---|---|
| boot to a prompt (total) | 2507 / 2516 / 2509 ms | 1790 / 1780 / 1786 ms |
| of which firmware (OVMF) | 1431 / 1429 / 1427 ms | none (`-kernel`) |
| **of which this kernel** | **1076 / 1087 / 1082 ms** | **1790 / 1780 / 1786 ms** |

The caveat was real, and it was most of the gap. On the part each project actually wrote, Aletheia
reaches an interactive prompt in ~1082 ms against ~1786 ms — about **1.65x faster** — while STILL
losing the total by ~0.72 s, because it pays for UEFI and the other leg does not. Both statements
are true and neither replaces the other; the table prints all three rows so a reader cannot take one
without seeing the others.

Named non-claims, in register: splitting a number does not win it — the TOTAL boot column is still
lost and still reported as lost, because a machine that boots through firmware takes longer to reach
a prompt than one handed the CPU, and that is the honest end-to-end experience. The two kernel
shares are far closer to like-for-like than the totals were but are NOT identical work: Linux's
1786 ms includes QEMU loading and decompressing a 14.16 MB kernel-plus-initramfs payload, while
Aletheia's 1082 ms starts from a 1.44 MB image firmware has already placed — part of the difference
is the payload difference, priced separately in its own row, and anyone quoting the 1.65x owes the
reader that sentence. This wave did NOT change the kernel: no boot path was optimized, the same
binary was measured more carefully.

## Previous wave — a parked machine costs nothing (2026-09-12, ADR-081)

The comparative benchmark exists so "faster than Linux" is a measurable statement rather than an
adjective. Run honestly, it said Aletheia **LOST** the idle column: 0.5% host CPU at the prompt
against Linux's 0.1-0.4%. That is one of the two genuinely fair columns in the whole table, so
there was nowhere to put the loss except on this kernel. Two things were wrong — the instrument
first, then the kernel.

**The instrument.** `boot_and_measure` polled for the prompt marker with `sleep 1`: one SECOND of
quantization on a two-to-three second measurement, so every boot time was rounded up toward the
next poll and a gap between two legs could be mostly the sleep. At 5 ms the same binaries measure
2487 ms against 1773 ms where the coarse poll reported 3065 against 2044 — both legs overstated,
the reported gap shrinking from ~1.02 s to ~0.71 s. Pre-fix numbers are NOT comparable to post-fix
ones and are retired rather than reconciled. The Linux leg also required Docker purely to obtain a
static busybox, so on a machine with no container daemon the comparison SKIPped and the claim
stayed unmeasured; it now builds the same busybox initramfs from Alpine's minirootfs with host
tooling, carrying the musl loader.

**The kernel.** `conirq::init` masked IRQ0 at the 8259A, which stops the interrupt being DELIVERED
but does not stop the 8254 from COUNTING. The emulator went on modelling a device ticking a hundred
times a second, and a host emulating a counter for a sleeping guest is a host burning CPU on behalf
of nothing. Masking answered "does the kernel get woken up"; the column was asking "does the
machine cost anything". `pit::quiesce()` reprograms channel 0 to mode 0 — interrupt on terminal
count, which does not reload — so the counter runs down once and stops. Not a slower tick: the last
tick.

Idle host CPU at the prompt went from 0.5% to **0.0%** across three runs, against Linux's 0.6%. The
column flipped because a real periodic cost was found and removed, not because a measurement was
chosen differently.

The table after both fixes (`docs/evidence/perf001`, three independent runs, same host, same
`qemu-system-x86_64`, same TCG mode, same `-machine q35 -m 256 -smp 4 -cpu qemu64`, both to an
interactive ttyS0 shell):

| Column | Aletheia | Linux 6.12-lts | Winner |
|---|---|---|---|
| boot to a prompt (total) | 2507-2516 ms | 1780-1790 ms | Linux, by ~0.72 s |
| idle host CPU at prompt | 0.0 % | 0.2-0.3 % | **Aletheia** |
| bootable payload | 1,439,744 B | 14,163,373 B | **Aletheia**, 9.8x |
| typed echo round-trip | 64-69 ms | 751-783 ms | **Aletheia**, ~11.3x |
| privileged lines of code | 42,078 Rust | ~40M C (cited) | **Aletheia**, ~950x |

Named non-claims, in register: four of five is NOT "Aletheia beats Linux". Boot time is still lost
by ~0.71 s, and Aletheia boots through OVMF while the Linux leg is `-kernel`-loaded and skips
firmware entirely — splitting Aletheia's total into firmware and kernel shares is the obvious next
rung and HAS NOT been done, so no part of that gap is currently excused. The round-trip win carries
a kernel-space/user-space asymmetry stated beside it (Aletheia's dispatcher is in kernel space;
busybox `sh` is user space over syscalls). The payload win is mostly a size difference, not a
design victory. NOTHING here measures security. NO other operating system is measured — Windows,
macOS, the BSDs and every RTOS are absent, and the Redox leg is opt-in and was skipped. And
`docs/MATURITY.md` still says plainly that nothing here is production-ready.

## Previous wave — the watch is wired to the clock (2026-09-12, ADR-080)

ADR-079 closed with a named non-claim: *nothing calls `tick` from a real timer interrupt yet*. This
wave closes that one. The resident governor is now driven by each target's real periodic interrupt,
on demand it measures from the machine's own busy/idle split, with nobody declaring anything on its
behalf.

Two failure modes come free with wiring a governor to an interrupt handler, and both are refused by
construction:

* **The lock is never waited on.** A handler runs on top of what it interrupted, so if the
  interrupted code held the watch lock, spinning would wait for code that cannot run until the
  handler returns — a one-core deadlock with no second core to blame. `SpinLock::try_lock` (new,
  and the only form a handler may use) turns that into `contended()`, a counted stand-down that is
  REPORTED, never gated: a nonzero count is a fact about the machine, not a fault in it.
* **An uncommissioned watch is a no-op.** The interrupt may be wired before the governor is stood,
  in either order; an early tick does nothing rather than crashing or acting on stale state. And
  `commission` refuses a second call, so a live governor is never silently replaced.

Demand comes from the machine: on x86-64 the IRQ0 handler reads an IDLE flag the boot path sets
around its `hlt`, so a tick that woke a halted core is an IDLE tick and one that interrupted working
code is a BUSY tick; on aarch64 and RISC-V the timer trap fires while a ring-3/U-mode task runs, so
the slice it closes is busy. Temperature is a fixed STAND-IN on every target and named as one in the
source — no target exposes a thermal sensor to a guest, and inventing a curve would be the thing
ADR-056 forbids.

What the machine prints — the same governor, the same boot, two opposite regimes:

```
[lethed] THE WATCH IS LIVE: 5 of 5 real IRQ0 ticks admitted, demand 0% measured, point index 0
[lethed] the watch under load: 14 of 14 ticks admitted, demand 100% measured, point index 2 of nominal 2
[lethed] THE ADVISOR IS CONSULTED LIVE: 1 consultations over 16 admitted real timer ticks
```

Core halted: measured 0%, governor at the lowest point. Core working: measured 100%, governor at
nominal and no further. Nobody supplied either number.

Gated are the contract's properties, never the numbers: the census balances, `pm_refusals == 0`, the
governor range is never left, measured demand is actually answered, and on x86-64 the advisor is
genuinely consulted on live measurements (boot fails 619/618/617/616/615/614 respectively). A
fifteenth boot invariant joins `lethed_suite` on all three targets — the machine-wide watch is stood
exactly once and is a no-op until it is — and the VirtualBox gate requires the marker too
(`lethed=15` on all four gates).

Named non-claims, in register: **the advisor reaches live consultation on x86-64 only.** aarch64 and
RISC-V arm their timer for the ring-3 run and disarm it after, so their watch sees six slices — under
one 16-sample window — and correctly reports itself still WARMING rather than claiming a
consultation it did not make. Giving those targets a free-running periodic tick is a separate rung.
Still no MSR/CPPC/ACPI frequency programming (QEMU TCG exposes no frequency control to a guest, the
ADR-071 posture). And still nothing about other operating systems: this says the governor is live,
measured and bounded on THIS kernel, not that its power management beats Linux, Windows or anything
else — no such comparison has been run.

## Previous wave — the advisor takes the watch (2026-09-12, ADR-079)

ADR-078 published a named non-claim: *no live governor thread exists yet*. This wave closes it.
`kernel-core/src/lethed.rs` is **the watch** — Lethe resident, running on the clock — and it closes
the gap without loosening a single bound.

The question a resident governor raises is not "what should the clock be" (ADR-078 answered that).
It is **who gets to make the machine act, and how often**. A governor driven by the timer interrupt
is reachable by anything that can influence when the timer fires, so the contract here is about the
TICK, not the clock:

* **The cadence is authority.** A tick is admitted only if it is strictly newer than the last
  admitted tick and at least `min_gap` beyond it. A replayed or rolled-back timestamp is
  `NotMonotone`, a too-eager one is `TooSoon`, a nested one is `Reentered` (the ADR-039 guard), an
  empty watch is `NoDomains`, an unusable cadence is `BadCadence`. A refused tick moves NO state —
  not the cursor, not the history, not the contract — and lands in a census that balances at every
  instant. A host proof fires 10,000 ticks at a floor of 1,000 and asserts exactly 10 admissions:
  churn, which on real silicon is energy and heat, cannot be amplified through this door.
* **A stale window is not a window.** Past the staleness ceiling the machine moved without us, so
  the governor RESYNCS — forgets every window, withholds the advisor until a full 16-sample window
  refills with post-gap truth, and says how many times it did so. The same rule covers cold boot,
  so the advisor is never consulted on a partially-filled ring: a feature built from six samples of
  sixteen is not a weak signal, it is a false one. Withholding is ADR-056 applied to time.
* **The work per tick is bounded, constant, and allocation-free.** Exactly one domain per tick,
  round-robin: one demand read, one sensor read, one depth-3 tree walk, at most one contract act —
  independent of domain count. The attached list is a fixed array claimed once, deliberately not
  `domain_ids()`, which allocates.
* **Demand is MEASURED, not declared.** `DemandMeter` turns busy/idle accounting into a percentage
  over disjoint windows whose counters RENORMALIZE rather than saturate — saturation would destroy
  the ratio and report a fully loaded domain as 1%, so an unconsumed window degrades in precision,
  never in truth.
* **The ceiling outranks the advisor.** While a thermal cooldown is latched the governor stands
  down, even though the contract would permit a raise inside the governor range, because raising
  silicon the thermal contract just clamped is how a machine oscillates at its trip point. It still
  parks a genuinely idle domain — the one act that can only help while cooling.
* **No new authority.** The resident holds no grant and offers no token, so the overclock band is
  unreachable BY CONSTRUCTION, not by policy.

Every act flows through `lethe::govern_one_advised` — ADR-078's sweep body, lifted unchanged, all
32 pre-existing `lethe` and `pm` proofs still green — so the resident inherits that wave's proofs
whole, including the one that matters most: with the advisor absent the advised path drives the
machine through the SAME clock sequence as the untouched ADR-076 baseline. `PmEngine::govern` is
byte-for-byte unchanged; `PmEngine::cooldown_remaining` became public, read-only, so the resident
can see the ceiling holding and stand down on its own.

Proofs: 15 host tests in `kernel-core/tests/lethed.rs` (adversarial clock streams that jump, stall
and run BACKWARDS with the census asserted to balance at every step; the berserk-timer rate limit;
stale resync and full-window rewarm; the advisor-free resident landing exactly on the baseline
demand map over 40 randomized trials; no point above nominal over 4,000 ticks; demanded silicon
never parked over 3,000 ticks; heat outranking the advisor for a whole 200-tick cooldown with
`pm_refusals == 0`; meter exactness across the entire 0..=100 range and under `u64::MAX` input;
capacity bounding; round-robin fairness), plus 14 invariants booting on all three targets
(`[lethed] ALL 14 RESIDENT GOVERNOR INVARIANTS HOLD`, boot fails 620+i, `lethed=14`), and seven
pinned cross-CPU in the conformance contract (159 -> 166 named behaviors).

Named non-claims, in register: this wave makes the governor RESIDENT, not HARDWARE. Nothing calls
`tick` from a real timer IRQ yet — the watch is built, proved and booted on three targets, and
wiring it to each target's timer interrupt and to the scheduler's busy/idle accounting is the next
rung, deliberately separate so the contract is proved before it is connected. The kernel still
programs no MSR/CPPC/ACPI frequency control (QEMU TCG exposes none to a guest, the ADR-071
posture), temperature is still reported by a caller rather than simulated thermodynamically, and
ADR-078's benchmark numbers still live in the trainer's documented cost model — they say nothing
about Linux, Windows, or any real operating system.

## Previous wave — Lethe, the resident performance advisor (2026-08-28, ADR-078)

The power/performance contract (ADR-076) made frequency AUTHORITY and heat a HARD CEILING; this
wave gives its governor a MEMORY that obeys it. `kernel-core/src/lethe_contract.rs` + `kernel-core/src/lethe.rs`
define the 12-feature contract (demand history over 16 samples, dwell at the current point,
churn in the last 16 steps, reported temperature against the trip margin, the point's share of
the governor range) and the advisor: two decision trees packed into `models/lethe_pm.alth`,
verified at load against ten named refusals — including a CYCLE check (load walks each tree
with a visited set, because an evaluate-time loop would be a hang, not an error) and an
inverted training box (the range guard must be able to fire). `govern_advised` observes the
live state EXCLUSIVELY (history strictly before the advice acts), consults the advisor, and
acts only through the contract's own named APIs — `request_index`, `wake`, `enter_idle` — so
every act is audited and every refusal named.

The advisor proposes; the contract disposes. The suite proves the sharp edges: with a
full-ceiling grant MINTED and the advisor decisive, no reachable point exceeds nominal; parks
happen only at zero demand; residency is monotone and wake latency is a sum of real wake costs;
the census accounts for every consultation; and with the advisor ABSENT — or abstaining on a
collapsed training box — the advised path drives the machine through the SAME clock sequence as
the untouched `govern` baseline. `PmEngine::govern` is byte-for-byte unchanged.

Proofs: 13 host tests in kernel-core/tests/lethe.rs (the full mutation table for every named
refusal, contract/blob agreement, fixture parity with determinism, the absent- and
abstaining-advisor equivalence sweeps over randomized multi-regime traces, the safety sweep,
engine-level determinism including the ledger, ledger wraparound, observer bounds with
features in-domain for arbitrary streams, degenerate-input withholding, a REPORTED advice-cost
measurement of ~370 ns/advice in a debug build), plus the 12 invariants booting on all three
targets (`[lethe] ALL 12 LETHE ADVISOR INVARIANTS HOLD`, boot fails 580+i), seven pinned
cross-CPU in the conformance contract. The marker maps changed deliberately (lethe=12,
ADR-061).

The benchmark proof is vendored (docs/evidence/lethe006): a deterministic trainer/exporter
(six workload regimes with a thermal stand-in that heats on the clock each arm ran at;
cost-sensitive depth-3 CART on expected per-row cost; K=16 class-consistent counterfactual
rollout labels; two DAgger rounds on the policy's own trajectory) plus the six-arm comparison
on 300 held-out traces — ADR-076 baseline, eager C2 parker, TUNED classic hysteresis, Lethe,
always-nominal, always-low — under a documented cost model (ramp 2 steps, wake penalties 1/3
steps, CV² energy with the ladder's own mV, unmet work weighted 10× energy with the α sweep
published). Lethe 0.6100 vs baseline 0.6280 (+2.88%) and hysteresis 0.6876 (+11.29%),
dominating the baseline on BOTH components; the per-regime decomposition shows the lead is the
idle policy (0.017 vs 0.097 on idle regimes) and Boost's anti-churn pinning (staccato 1.017 vs
1.083), with the bursty regime honestly LOST (0.869 vs 0.857). Named non-claims, in the
register: the numbers live in the simulator's cost model (the kernel models transitions as
free); no live governor thread exists yet (residency = wired into the model's govern path and
proved at boot, the pre-REQ-ML-003 posture); the corpus is synthetic — this says nothing about
real silicon or real operating systems.

## Previous wave — the composition contract is modeled, not assumed (2026-08-28, ADR-077)

ALET-P2-021's compositor rung. The GUI question decomposes into the two questions this kernel
already knows how to answer: WHO may put pixels on the scanout (an authority question) and
WHERE those pixels may land (a bounds question). `kernel-core/src/compositor.rs` defines the
contract as a complete software model: surfaces are minted with unforgeable possession-based
OWNER tokens and every op — attach, move, raise, lower, detach, and each pixel write — is
refused `NotOwner` without the right one; placements are clipped to the scanout EXACTLY
(hangs off any edge -> intersection only; could-never-show-a-pixel placements refused at
attach AND move; the write loops only visit pixels that exist in both surface and scanout —
the host proofs run against a GUARD-BAND raster whose out-of-bounds-put counter must stay
zero); the painter's order is the z-order and only the owner may change it; packed buffer
fills are SIZE-HONEST (short can never overread, long can never smuggle, refused fills leave
the surface untouched); placement changes are VISIBLE the same frame — attach/move/detach/
raise/lower damage the screen regions they vacate and cover, and compose clears each damaged
region to background before repainting it through the z-order, so a moved surface leaves no
ghost and a damaged bottom surface cannot paint over the windows above it; damage ledgers
are bounded and coalesce (summarized, never lost); an unchanged frame visits NO region and
writes ZERO pixels, with every frame's cost REPORTED (`FrameStats` — the measured shape of
"maximum performance", ADR-064's posture); and identical op sequences land bit-identical.

Proofs: 11 host tests in kernel-core/tests/compositor.rs (four-edge clip sweep with
per-pixel oracles, the ownership table over every op, the buffer-honesty matrix,
placement-damage visibility, exact damage accounting, bounds/capacity, token non-reuse,
determinism) plus 14 in-kernel invariants booting on all three targets (`[compositor] ALL 14
COMPOSITION-CONTRACT INVARIANTS HOLD`, boot fails 600+i), six pinned cross-CPU in the
conformance contract (146 -> 152). Marker maps changed deliberately (`compositor=14`,
ADR-061). Named non-claims, in the register: no real-pixel compositor leg over the virtio-gpu
flush path yet (QEMU's virtio-gpu can SHOW a frame but enforces nothing about who composes
it — the ADR-071/076 posture), no alpha blending beyond the 1-bit depth the framebuffer
console already runs, no cursors, no input routing, no device-level GPU isolation between
surfaces.

## Previous wave — the power/performance contract is modeled, not assumed (2026-08-28, ADR-076)

ALET-P2-022 leaves the deferred column. The wave answers the OS's overclocking promise the way
this kernel answers every privileged act: frequency is AUTHORITY, heat is a HARD CEILING.
`kernel-core/src/pm.rs` defines the contract as a complete software model: every core belongs to
a frequency DOMAIN with an honest discrete ladder (registration refuses dishonest ladders by
name); the governor range (at or below nominal) is free to any caller; the OVERCLOCK band above
nominal exists only through a LIVE, per-domain elevation grant — attenuated on delegation
(a child ceiling never widens its parent, `Amplification`/`CrossDomain` refused), revoked with
cascade, and clamping the domain back to nominal the moment its grant dies (a governor-range
grant clamps nothing); the thermal ENVELOPE is absolute BY CONSTRUCTION — no ladder point above
it can register and no grant past it can mint, so no reachable state exceeds it, whatever
authority says; a thermal TRIP clamps every domain to its lowest point and latches a tick-exact
cooldown that refuses elevation BY NAME even with a valid grant while the governor range keeps
serving; the demand governor never enters the OC band and never parks demanded silicon
(`DomainBusy`), parking zero-demand domains instead (the idle machine costs nothing, ADR-056);
idle residency and wake latency are accounted exactly, with a clock change CLOSING a parked
span so real time is never lost; device power moves only along legal arcs (D3→D1 refused — wake
through D0 or not at all); and every accepted act and every refusal lands in a bounded audit
ledger under a monotonic sequence, the holder named on grant acts.

Proofs: 19 host tests in kernel-core/tests/pm.rs — the full OC-band decision table over every
point × ceiling × authority state, a 5^3 attenuation-chain sweep, revocation clamps and
idempotence, envelope absoluteness from registration and mint, cooldown tick-exactness across
the whole window, idle accounting under transition interference, the complete device-arc table,
ledger completeness with wraparound, capacity bounds, and bit-identical determinism — plus 14
in-kernel invariants booting on all three targets (`[pm] ALL 14 POWER-PERFORMANCE INVARIANTS
HOLD`, boot fails 560+i), six of them pinned cross-CPU in the conformance contract. The marker
maps changed deliberately (`pm=14`, ADR-061). Named non-claims, in the register: no
MSR/CPPC/ACPI programming (QEMU TCG exposes no frequency control to the guest — a hardware rung
attempted today could only prove code ran, not that anything enforced; the ADR-071 posture),
no battery, no system sleep/wake, no voltage rail enforcement beyond recording mV, no
thermodynamic simulation — callers report temperatures, the contract decides.

## Previous wave — per-device DMA windows (2026-08-26, ADR-075)

ALET-P1-018 advances to its third hardware rung. The registry-driven narrowing lands on VT-d:
every DRIVEN function now gets its OWN second-level tree containing exactly the frames ITS
driver registry vouches for (leaf-set equality audited live against sorted spans), ungranted
functions get NO context entry and the gate reads their absence back from the live context
table, grant sets are pairwise disjoint or the boot refuses, and revocation granularity drops
to ONE PAGE: the block device data-frame leaf is revoked under enforcement and the unit answers
with an ACTIVE record naming source-id AND address with MEASURED reason 6 (PAGING_NOT_PRESENT,
pinned beside the ADR-073 codes 2/4/5) while sibling windows keep serving; restore returns
read-back equality and silence; enforcement stays latched layered over the software registry.
dmar 12 -> 14; host proofs tests/vtd.rs 12 -> 15. En route the wave EXPOSED and FIXED a
repo-wide boot breaker: the ADR-074 seam mapped [bar_base, len) instead of bar_base+offset, so
q35 device-cfg ran unmapped and every target gate died in an infinite mis-labelled ring-3 fault
loop (commit fix(pci), found by bisect plus CR3/translate instrumentation). Named at the
boundary: SMMUv3 per-stream windows, device-side walk probes on ARM (QEMU 11.1 artifact),
interrupt remapping, queued invalidation and pass-through types stay open in the gap register.
## Previous wave - the IOMMU contract crosses the ARM fence (2026-08-26, ADR-074)

ALET-P1-018 advances to its second hardware rung. DELIVERY on aarch64: kernel_core::smmu programs
the ARM SMMUv3 QEMU emulates on virt - discovered through the machine's own device tree, delivered over
the firmware configuration channel (the same door as the custody anchor; direct -kernel ELF boots get
NO DTB pointer at all - measured x0=0), stage-2-only identity domain over OWNED frames minus image, every
present PCI function granted an STE under its DECLARED iommu-map stream id, stream table + command/event
queues published with readback, enforcement enabled through CR0->CR0ACK and latched layered over the software
DMA registry: a 10-invariant boot gate (smmu=10) on top of 15 host proofs against a simulated unit and a
device-side walker built from the emulator's own decoder shapes. The virtio-pci transport moved ONCE into
kernel-core (PciEnv seam) when this wave became its second consumer; the aarch64 kernel became its own PCI
firmware (BAR sizing + assignment) because bare-metal boots run none. NAMED at the boundary: CLI-attached
virtio-pci DMA does not traverse the legacy iommu=smmuv3 unit on QEMU 11.1 (abort-canary measured), so
grant-serves/revocation-events stay open in the gap register beside ADR-073's completion-loss artifact.


## Previous wave — the IOMMU contract is programmed into real silicon (2026-08-25, ADR-073)

ALET-P1-018 advances to the first hardware rung. DELIVERY on x86-64:
`kernel-core/src/vtd.rs` (the wire: register map, root/context encodings, second-level domain
builder, auditor, controller with named refusals) + `kernel-x86_64/src/vtd.rs` (the platform:
ACPI DMAR/DRHD discovery, UEFI-map spans minus the kernel image, per-bus-0-function context
entries, and a 12-invariant live gate). The gate adopts the root (SRTP), turns enforcement ON
(TES observed), kicks the LIVE block functions this boot already drives, and takes its evidence
from the unit's own fault bank: the granted function walks CLEAN; revoking a function's context
and kicking produces an ACTIVE record naming its source-id with reason CONTEXT_ENTRY_P; restoring
the grant returns that function to silence; enforcement stays latched until halt
(`[dmar] ALL 12`, marker dmar=12). Drivers negotiate VIRTIO_F_IOMMU_PLATFORM whenever offered.
Two register-interface facts were forced by the live unit and are documented in ADR-073: the
fault bank is WRITE-ONE-TO-CLEAR, and QEMU serves FSTS at 0x34 where the spec puts 0x30 — so
enforcement EVIDENCE comes from the fault-record BANK (exact everywhere), not from FSTS.PPF.
Boot order changed deliberately: every DMA-dependent suite runs BEFORE the vt-d gate (devices are
brought up before enforcement — how real platforms meet an IOMMU) and the gate is last, because
what it turns on stays on until halt. Named non-claims: SMMUv3 delivery, per-device windows,
interrupt remapping, queued invalidation, pass-through types, and post-enable completion
assertions — QEMU 11.x TCG loses virtio completions across a mid-run enablement ('bogus descriptor
or out of resources'); the full evidence trail is in ADR-073.

## Previous wave — the custody anchor crosses the platform boundary (2026-08-24, ADR-072)

ALET-P1-034 closes completely. DELIVERY: \`kernel-core/src/bootroot.rs\` + per-target fw_cfg
transports hand the vault its 32-byte root over the platform channel; only Delivered(exactly 32)
opens a vault, and RootNotProvided / FirmwareAbsent / MalformedRoot are refused BY NAME. DECISION:
image and entity store stay two commits but are mutually detectable — each paired commit writes
the vault generation inside the durable entity record, and custody-open enforces
witnessed_generation <= keystore_counter, converting ADR-070's pinned undetectable residual into
a named refusal. Proofs: host sweeps in tests/bootroot.rs (lying directories, truncations,
wrong sizes, constructed pair-rollback, fault-at-every-pair-position) plus [vault] ALL 14
CUSTODY-DELIVERY INVARIANTS HOLD on real firmware + real persistent media on all three targets.
Every QEMU gate gained a THIRD rootless boot proving absence seals the vault while the machine
continues; marker maps gained vault=14 deliberately (ADR-061). Heap grew 8 -> 12 MiB on the DT
targets to hold the resident custody state (ADR-063 posture).

## Previous wave — the IOMMU contract is modeled, not assumed (2026-08-23, ADR-071)

ALET-P1-018 advances: `kernel-core/src/iommu.rs` defines and proves the full enforcement semantics
of a hardware IOMMU as a software model (`SoftIommu`), so every proof runs on the host today and
a hardware implementation must satisfy the same contract. Nine invariants boot on all three
targets; seven are pinned cross-CPU. The gate-marker map changed deliberately (`iommu=9`).
Hardware realization (VT-d/SMMUv3 programming) stays scoped in the gap register.


## Current wave — authority custody is a lifecycle, not a caller-supplied key (2026-08-23, ADR-070)


The custody and rotation halves of ALET-P1-034 close, because they were one gap: `capstore` could
authenticate a persisted registry only under a key the CALLER handed in on every call, so custody
was nobody's, rotation was impossible, and every boot re-asked the question a keystore exists to
answer. The constraint that shaped everything: the kernel has NO entropy source at boot, so
randomness could not be the mechanism — the lifecycle had to be safe BY CONSTRUCTION.

* **The root is custody; working keys are derived.** `CapVault::open` takes the 32-byte root once,
## Gates executed in CI

Both pipelines (GitHub Actions and GitLab CI) execute exactly these scripts, each asserted by
scripts/check-ci-parity.sh against this file: scripts/build-all.sh (every crate on its own
toolchain, host crates tested), scripts/check-boundary-docs.sh, scripts/check-ci-parity.sh,
scripts/check-register.sh, scripts/check-traceability.sh, scripts/comparative-bench.sh,
scripts/conformance.sh (the cross-CPU core contract), scripts/console-agent-e2e.sh,
scripts/console-ai-e2e.sh, scripts/console-e2e.sh, scripts/keyboard-e2e.sh,
scripts/quality-gate.sh, and the four VM gates — scripts/vm-e2e.sh (aarch64),
scripts/vm-e2e-riscv.sh (RISC-V), scripts/vm-e2e-x86.sh (x86-64 under OVMF) and
scripts/vm-e2e-vbox.sh (VirtualBox, the second-hypervisor rung).
