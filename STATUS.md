# Aletheia — Implementation Status

**As of:** 2026-08-10 21:39 +08 (risk-advisor VM-gate wave)
**Milestone delivered:** M1 — Hosted System-Core Reference (Rust); **P2 (start)** — WASM capability-secure component runtime; **P4 (start)** — bootable microkernel on THREE CPU targets, VM-tested: aarch64 (bootstrap) + AMD64/x86-64 (first-class) + **RISC-V/RV64GC (first-class)**; **P5 (start)** — real memory management: physical page-frame allocator + MMU virtual memory (identity map + dynamic map/unmap) + **EL0 user-mode with a capability-gated syscall boundary, hardware address-space isolation, per-process address spaces (separate TTBR0), and preemptive multitasking (full trap-frame context switch + round-robin scheduler + GICv2/generic-timer IRQ preemption)**, VM-tested on the aarch64 dev backend
**Maturity:** `docs/MATURITY.md` grades every subsystem Proved / Implemented / Architecture and states
plainly that **nothing here is production-ready** — read it before quoting any claim below.
**Sources of truth:** `docs/Aletheia_Product_Requirements_Document.md` (PRD-003),
`docs/Aletheia_Software_Architecture_Document.md` (SAD-002), `docs/adr/ADR-001..056`.

## The risk-advisor VM gate (2026-08-10 21:39 +08) — the forest is on the boot path of all three targets

The previous wave landed the in-kernel risk forest (REQ-ML-001, ADR-056) and said so honestly: the
traceability row read *"hosted; no VM gate — the advisor is not on any target's boot path yet"*. A
model proved only on the host is a model no booted machine has ever verified. This wave closes that,
and the closure is a gate, not a paragraph.

* **The blob is part of the image.** `kernel_core::mlrisk::BUNDLED_MODEL` embeds
  `models/aletheia_risk.altm` with `include_bytes!`, so the bytes the kernel evaluates are covered by
  whatever attests the image and a running kernel cannot be holding a model its own artifact hash
  does not account for. Embedding is not trusting: every target calls `RiskAdvisor::load` **at boot**
  and prints what it got — `[mlrisk] bundled forest: 119 trees, 6367 nodes, worst case 714 compares
  per advice` — or the named `ModelError` that refused it. A refusal is printed, never inferred from
  silence.
* **One suite, three targets, twenty invariants.** `mlrisk::mlrisk_suite` is arch-independent in the
  same sense as `selftest::run`: the invariants and their names are defined once and each target
  formats the lines with its own `kprintln!`. It proves, in kernel space, against the image's own
  bytes: the model verifies; the hot-path cost is a bound **measured** by walking the shipped table;
  every margin, verdict and range-guard flag over the **whole** committed fixture matches the
  trainer **exactly**; evaluation is deterministic; an input outside the training box abstains;
  a minimal well-formed blob loads and evaluates as built; and each of the nine ways a blob can be
  wrong (short, magic, version, feature count, fixed-point scale, moved feature contract, truncated
  or over-long, empty forest, backwards or out-of-range child) is a **named** refusal.
* **The advisory property is proved by observation, not by code shape.** Invariant 18 runs a
  model-free `PriorityScheduler` and an abstaining-model one over the same admissions and requires
  the drained orders to be **identical**; 19 shows a decisive verdict reordering two tasks of EQUAL
  priority; 20 shows a higher-priority high-risk task still running before a lower-priority low-risk
  one. Priority is never traded for risk, and that is now asserted on real hardware paths rather than
  only on the host.
* **The refusals cost a few hundred bytes, not a megabyte.** The corruption checks mutate a
  synthetic 400-byte blob built in-kernel rather than copying the 100 KiB bundled one, because two of
  the three targets allocate from a bump allocator that never reclaims. Check 8 loads that synthetic
  blob successfully first, so every refusal after it refuses a *specific* corruption instead of a
  blob that was never acceptable.
* **Gated, everywhere.** `scripts/vm-e2e.sh`, `scripts/vm-e2e-riscv.sh` and
  `kernel-x86_64/scripts/smoke-test.sh` now require `ALL 20 RISK-ADVISOR INVARIANTS HOLD`; the hosted
  `tests/mlrisk.rs` pins the count at 20 and asserts `BUNDLED_MODEL` is byte-identical to the file the
  hosted parity tests read, so the VM gate can never be proving something about bytes no test has
  seen. `./scripts/e2e-all.sh` is green on aarch64 + RISC-V + x86-64 (VirtualBox rung SKIPs: an arm64
  host cannot virtualize x86-64). `kernel-core` hosted suite **322 passed** (28 suites);
  `clippy -D warnings` clean (this wave also fixed a `match_like_matches_macro` lint the previous one
  left in `priosched.rs`); `check-traceability.sh` and `check-ci-parity.sh` PASS — the traceability
  gate was **failing** on the previous commit because the REQ-ML-001 VM-gate cell held prose instead
  of script paths, which is exactly the failure the gate exists to catch.

**Still open, named:** the suite is on the boot path; *real-task feature extraction* is not. Deriving
the 20-feature vector from live kernel tasks needs feature meanings the kernel does not yet measure,
and inventing them would make the parity fixture a decoration. Until then the forest is verified,
bounded and proved advisory on every boot, and the scheduler's tiebreak uses it only where a caller
supplies advice. Loading the blob from a capability-scoped file with a signature check remains
deferred by ADR-056.

## The console-agent wave (2026-08-10) — one request becomes a session, on three CPU targets

The previous wave left two things written down as open, and this one closes one of them completely
and refuses to pretend about the other.

* **ALET-P2-047 — a request is a bounded SESSION now, not a single command (REQ-AI-007, ADR-054).**
  `console.rs` planned one command, which is right for an interpreter and a ceiling on the surface: a
  request whose answer is not visible in the namespace listing is not a harder one-command request,
  it is not a one-command request at all. `aletheia/src/ai/agent.rs` makes it a loop — propose,
  validate, render, type, observe, propose again — in which the model reads **the machine's answer to
  its own previous command**. That deleted the `case "$planned" in write*|rm*|…` list of verbs the
  previous gate used to guess when the model's picture had gone stale: a second copy of the kernel's
  command table, in the one language nothing here tests. Every ADR-053 guarantee is re-applied per
  step, and three bounds exist only because it is a loop: a step budget, no-progress detection, and a
  refusal to end the session it is observing (`halt` refused **even with approval** — an agent that
  stops the machine cannot read the result of stopping it).
* **The other half of ALET-P2-047: the model arm now runs on all three CPU targets.**
  `scripts/console-agent-e2e.sh` is three legs — aarch64, riscv64, x86-64 under OVMF. The same
  dispatcher running on all three was never a reason to believe the *planning path* had been driven
  on more than one.
* **ALET-P2-048 — four defects a live model found, and none of them were the model (REQ-AI-008/009/010,
  ADR-055).** The loop passed its deterministic arm first time and then failed **all three** cases
  against a real model. (a) It proposed `cat` with a two-word name and the session died on a refusal
  Aletheia knew the words for — *"cat: name must be one word"* — and told nobody; recoverable
  refusals are now **corrections** that enter the transcript and get re-asked, bounded at three and
  never charged to the console budget, split from real bounds by `Refusal::is_recoverable` (which the
  previous wave defined and never called). (b) It read an object, changed it, read it again, and was
  refused for "no progress" that was no longer true — the same "assume the machine is static" defect
  the wave accused the old shell list of, in Rust. (c) The first cut of that fix let it repeat the
  **change** instead, growing an object from 25 bytes to 49. (d) It answered correctly with `stat`
  and the gate failed it for not being `wc`; `must_type` is now a set of alternatives, each of which
  must be both typed and confirmed by what the console printed.

**Measured, this workstation** (LFM2.5-2.6B-Q4_K_M, llama.cpp `--jinja`, `-c 8192`, 27 tools
offered), `scripts/console-agent-e2e.sh`, **all six arms PASS** — 3/3 cases on each of three CPU
targets driven by a real model at a live console, plus four bound assertions and a power-cycle leg on
each. Median 7.2–8.6 s per model-arm turn; the deterministic control arm is 0 ms.

## The performance wave (2026-08-10) — an idle machine cost a whole core (ADR-056)

* **REQ-CON-006 — the console stopped spinning.** `kernel_core::shell::run_loop` read `let Some(byte)
  = getc() else { continue }`, so a machine parked at a prompt with nobody typing asked the input
  ring for a byte, was told there was none, and asked again, forever, on every core. Measured on this
  host: **91.8% of a host CPU, idle.** It had been wrong since ADR-045, because console input arrives
  by *interrupt* and the loop already had something to wait for. `ShellHost::idle` parks on it —
  `wfi` on aarch64/RISC-V, `sti; hlt` on x86-64 — and **defaults to doing nothing**, so a polled
  target can never be parked forever. **91.8% → 0.9%.** All three targets still PASS
  `scripts/console-e2e.sh`.
* **REQ-PERF-001 — a comparison on ONE substrate.** `scripts/comparative-bench.sh` boots Aletheia and
  a real Linux 6.12-lts kernel under the *same* `qemu-system-x86_64`, same host, same
  `-machine/-m/-smp/-cpu`, same TCG, to the same end state: an interactive shell on `ttyS0` blocked
  on input.

  | | Aletheia (x86-64) | Linux 6.12-lts |
  |---|---|---|
  | boot to a prompt (median of 3) | 3079 ms *(3058, 4082, 3079)* | 3048 ms *(2040, 3048, 3072)* |
  | idle host CPU at prompt | **1.2 %** | 1.7 % |
  | bootable payload | **523 776 B** | 13 895 205 B |
  | privileged lines of code | **22 557 (Rust)**, 302 `unsafe` | ~40 M (C, cited, not measured) |

  **No boot-time winner is claimed, and how that number behaved is the most useful thing in this
  section.** The script's commentary first predicted Aletheia would win. The first run said Linux, by
  2053 ms to 4068. The second run, same host and same binaries, said 3070 to 3080. One sample had
  been read as a result — twice, in opposite directions — and both readings were written into a
  commit message before the third run caught it. Boot time is now a **median over repeated runs with
  every sample printed**, and at 3079 against 3048 with that spread, the honest statement is that
  this benchmark does not distinguish them. One structural asymmetry is real and is Aletheia's to
  own: it boots through OVMF, a full UEFI firmware implementation, while the Linux leg is loaded
  directly with `-kernel` and skips firmware entirely — a boot-*path* difference, not a kernel-speed
  one.

  The two columns worth arguing about are the last two. **Idle CPU** is a fair fight — identical work
  (none), identical emulator — and parity with Linux is the claim. **Privileged lines of code**
  depends on no emulator, no host and no workload: it is how much code must be correct for the
  machine to be correct. The script prints what Aletheia **loses**, in its own output, and carries an
  attribute table for Redox, seL4, Theseus and Hubris with no numbers attached, because a number from
  somebody else's benchmark on somebody else's hardware is the exact thing it exists to refuse.
* **A theory that was wrong, kept because it is instructive.** The agent gate's ~8 s turns looked like
  contention from four spinning vCPUs. After the idle fix took the guest to 0.9%, the medians were
  re-measured: 8567/7204/8172 ms against 9877/7849/7768 before — unchanged inside the noise. Per-call
  token accounting found the real cost: a tool-call turn is ~224–353 completion tokens and 1.7–2.3 s,
  an *answer* turn is **859–1121 completion tokens** for one short sentence because LFM2.5 emits a
  long `reasoning_content` first, and the ~2000-token prompt is prefix-cached so prefill is not the
  cost. **Aletheia's own overhead is not measurable next to the model's generation time.** Sending
  `enable_thinking: false` was tried and changed nothing (296/224/353 tokens either way); recorded as
  a negative result in ADR-056 so nobody spends the afternoon on it twice.

* **The GitLab pipeline is gone, and the gate that depended on it was rewritten rather than
  weakened.** Aletheia is published to **GitHub only**. `.gitlab-ci.yml` is deleted, and
  `check-ci-parity.sh`'s check [2] — which required the two pipelines to execute the same script set
  — could not simply be dropped: a parity check with one side deleted is a check that always passes,
  which is worse than no check because it reads like one that is working. It now asks the question
  parity was a proxy for: **is there a gate script in `scripts/` that nothing runs?** Every script is
  either executed by CI or exempt *with a stated reason*. Wiring it found one real gap immediately —
  `build-example-component.sh`, which regenerates a committed fixture — now exempt on the record.
  `console-agent-e2e.sh` and `comparative-bench.sh` are wired into GitHub CI; both run their
  deterministic/Aletheia arms on a runner with no weights and no model, and SKIP the model and Linux
  legs loudly. `check-ci-parity.sh`, `check-register.sh` and `check-traceability.sh` all PASS.

## The console-planning wave (2026-08-10) — the model reaches the machine you sit in front of

The previous wave's benchmark ended by printing what it had *not* measured: the six hosted
operations, **NOT** the twenty-seven-command kernel console, with "no path from one to the other in
this build". That sentence was true and it described the hole. Aletheia had two command surfaces and
intelligence attached to one of them.

* **ALET-P2-044 — the console becomes a planning surface, from the kernel's own table.** A second
  operation family, disjoint from the entity operations by construction (`entity.derive` and `grep`
  share no vocabulary), derived from `kernel_core::shell::COMMANDS` — the table the dispatcher and
  `help` are both generated from — rather than retyped. A command cannot be added to the kernel
  without appearing in the model's menu. The hosted side adds what the kernel cannot know: an
  exhaustive risk classification (writes to the medium and stopping the machine are `Destructive`;
  an unclassified command fails a test) and a rendering contract in which a validated step becomes
  **exactly one console line**. A control byte in any argument is a refused plan, so a model
  argument cannot become a second command with the first one's authority. **ADR-053.**
* **ALET-P2-045 — five defects only a live model could expose, and none of them were the model.**
  Under a permissive JSON schema LFM2.5 looped argument keys until it exhausted the generation
  budget and returned empty on three of eight cases; caught in that raw output, it was trying to
  escape into `<|tool_call_start|>[write(name='notes', …)]` — it had produced the right call and the
  decode would not let it emit the format it was trained on. The console path now speaks **native
  tool calling**, and `spawn_llama_server` passes `--jinja`, without which `llama-server` never
  parses a tool call and a correct answer reads as no answer at all. A prompt clause saying *"only
  call `ls` when…"* took the score from 6/8 to **3/8**, because the negation is not what survives
  contact with a small model — the token is; the system prompt now names no command, enforced by a
  test. And with no context brief the model answered `ls` for anything needing a file, its own
  reasoning saying *"Let me first look at what files are available"* — correct for an agent, wrong
  for an interpreter, and fixed by letting it see. That is ADR-018 arriving on a second surface.
* **The loop is closed at a booted machine.** `scripts/console-ai-e2e.sh` boots the interactive
  kernel, types `ls` and reads the namespace **off the live guest**, asks in plain English, validates
  what the model chose against the kernel's own table, types the rendered line, asserts what the
  console printed — then power-cycles and requires that what was written survived and what was
  removed stayed removed. Two arms: the deterministic control arm needs no model and gates the whole
  pipe in CI; the model arm SKIPs, loudly, when nothing is serving the selected model.

**Measured, this workstation** (LFM2.5-2.6B-Q4_K_M, llama.cpp `--jinja`, `-c 8192`, 27 tools
offered): the console surface went **4/8 → 3/8 → 4/8 → 3/8 → 5-6/8 → 8/8**, and 8/8 held on two
consecutive runs at a median of ~800 ms, against a deterministic control arm at 8/8 and 0 ms. The
live gate passes both arms including the reboot leg. Register **83 → 87 findings, 48 → 50 resolved**;
traceability **92 → 93 requirements**.

**Not claimed.** There is still **no inference engine in kernel space** — `kernel-core` remains
`no_std` with no network, the model runs on the host, and what crosses into the guest is a line of
ASCII indistinguishable from one a person typed. 8/8 is eight cases, one machine, one quant, one
model: a floor for reproducing the setup, not a claim that arbitrary English drives this OS.
Approval is a CLI flag, not the Core's human-in-the-loop surface (**ALET-P2-046**, open). The live
gate drives aarch64 only; the other two targets run the same dispatcher under `console-e2e.sh` but
have never been driven by the model (**ALET-P2-047**, open). `docs/MATURITY.md` still governs every
claim above.

## The AI wave (2026-08-10) — the OS gets to choose its own mind, and gets measured

Everything this repository proved about the AI subsystem was a claim about **shape**: the provider is
model-agnostic, the plan is validated, the model never executes. Two things were never settled.
*Which* model — that was two `const`s and a manifest no code read, so changing the OS's intelligence
needed a compiler. And whether the resident model can actually plan Aletheia's operations at all —
a question that had never been asked, so it had never been answered.

* **ALET-P2-042 — the model becomes a system property, and the catalog is DISCOVERED.** Aletheia
  scans the local model cache and lists what is really on the machine — including models no manifest
  and no line of source has ever named, marked `unpinned` so it is clear their parameters are
  defaults rather than findings. Manifests (`models/*.toml`) *characterize*; they do not enumerate.
  Selection is persisted, a unique prefix is enough to type, and `aletheiad model list | use |
  status | pull | bench` is the surface. **LFM2.5-2.6B (Q4_K_M) is the new default**, with a sha256
  and size that were *measured* — and now **verified**: `model status` streams the file and reports
  `verified` / `MISMATCH` / `not pinned` / `unreadable` as four distinct outcomes. MiniCPM is
  retained, not deleted — a benchmark whose baseline has been deleted is not a baseline. **ADR-052.**
  *(The first cut of this got it wrong: it replaced two `const`s with a hardcoded list of three
  manifests and called that a registry. That is the same defect one level up — a guess about
  somebody else's disk — and it was rewritten to scan.)*
* **Aletheia's own model is registered before it exists.** `aletheiad model use aletheia-lm` works
  today and reports `NOT YET TRAINED`, naming the variable that will point at the finished weights.
  The switch is ready the moment pretraining is, and selecting it early cannot silently serve
  something else.
* **ALET-P2-043 — three defects only a live model could expose.** The health probe had tried only
  the **first resolved address** since ADR-017: `localhost` resolves to `::1` before `127.0.0.1`,
  `llama-server` binds IPv4 only, so a *running* model was indistinguishable from no model at all and
  the Core fell back to the deterministic interpreter without a word. The GBNF grammar — written for
  MiniCPM — made LFM2.5 return an **empty completion for all six operations**, because its chat
  template opens with a token the grammar has no rule for. And `n_predict = 512` truncated a
  schema-constrained decode mid-whitespace, presenting as "the model cannot plan this operation".
* **The benchmark refuses before it measures.** The endpoint is a *port*, and on the machine this was
  written on another project's service already held `:8080`. `model bench` asks the backend what it
  is serving and will not record a number until the answer matches the selected manifest.

**Measured, this workstation** (LFM2.5-2.6B-Q4_K_M, llama.cpp, `-c 8192`): the operation surface went
**0/6 → 2/6 → 6/6**, and 6/6 held on two consecutive runs, median 3.5–3.9 s per interpretation
against a deterministic control arm at 6/6 and 0 ms. Register **81 → 83 findings, 46 → 48 resolved**;
traceability **90 → 92 requirements**.

**Not claimed.** The benchmark drives the hosted Core's **six operations**, NOT the 27-command kernel
console: that dispatcher is in kernel space, in a `no_std` crate, with no inference engine underneath
it and no path from one to the other in this build. It keeps its own gate, `scripts/console-e2e.sh`.
One machine, one quant, one backend — a floor for reproducing the setup, not a benchmark of the
model. And the manifests *record* a checksum without yet enforcing it at load time; that is the
natural next row. `docs/MATURITY.md` still governs every claim above.

## The console wave (2026-08-09) — what a person sitting at the machine gets

Two defects, both reported by someone using the OS rather than by any gate here, and both closed:

* **ALET-P2-040 — the arrow keys typed into the line.** ADR-044's editor was a filter over single
  bytes and a terminal sends sequences: `ESC [ A` lost its `ESC` and typed `[A` into the middle of
  whatever was being written. No gate could have found it, because every console gate typed
  *characters*. Now: a bounded CSI parser (nothing inside a sequence reaches the line, the parser is
  never left armed, parameters are counted not buffered), a real cursor with mid-line editing, a
  bounded history, and Tab completion against the live namespace. **ADR-050.**
* **ALET-P2-041 — twelve commands is not a system you can work in.** Fifteen more, each a different
  path through machinery that already existed: `ver`, `lsblk`, `find`, `head`, `wc`, `grep`,
  `hexdump`, `append`, `touch`, `cp`, `mv`, `sync`, `history`, `clear`, `reboot`. **ADR-051.**

Measured, on all three CPU targets: console invariants **15 → 40**, input-ring **8 → 9**,
keyboard-decode **10 → 12**, the cross-target conformance contract **112 → 118** named behaviors,
`keyboard-e2e.sh` **7 → 11** checks (four of them pressing keys that used to corrupt the line), and
`console-e2e.sh` now drives the working command set at a live console and re-reads what `append`,
`cp` and `mv` produced *after a reboot*.

**Machine size.** The x86-64 image is gated on Oracle VirtualBox at **2 vCPUs and 512 MiB**, and
that is now what `scripts/vbox-install.sh` provisions by default. The VirtualBox rung boots the same
image at 512 MiB and at 1 GiB, because the firmware memory map is an input and one size is one map.

## Active triage execution queue (2026-08-10, after the console-planning wave)

Open GAPS4 backlog count from `docs/gap/ARCHITECTURE-GAPS4-REGISTER.md`:

- **P0 open:** 0
- **P1 open:** 11  (unchanged: the console-planning wave touched no P1, exactly as the AI wave did not)
- **P2 open:** 14  (`ALET-P2-044`/`045` opened and closed; `ALET-P2-046`/`047` opened and left open)
- **P3 open:** 3

The queue below is unchanged by this wave, deliberately, for the second wave running. The console
planning work was the same separate axis — the model subsystem — and none of it advances the crypto
trio at the front of the queue. Saying so is cheaper than letting a reader infer that a busy wave
moved the security backlog, which it did not. Two of its own rows stayed open rather than being
folded into the rows that closed: `ALET-P2-046` (approval is a flag, not a human) and `ALET-P2-047`
(one CPU target, one command per plan).

Execution order (wave-based, 2-3 tightly related P1 rows per wave):

1. **Security model completion (P1):** ~~`ALET-P1-026`~~, ~~`ALET-P1-027`~~ closed; next
   `ALET-P1-028` (key management), `ALET-P1-029` (nonce/IV lifecycle), `ALET-P1-030` (encrypted
   content-addressing identity). The crypto trio is now the front of the queue **and** the gate on
   two named non-claims this wave created: the capability image is checksummed rather than
   authenticated (ADR-048), and signing it needs a key whose own lifetime is `ALET-P1-028`.
2. **Capability store on the medium (`ALET-P1-034`, new this wave):** committing the ADR-048
   image through `persist.rs` — a filesystem ordering question (does the capability store commit in
   the same transaction as the entities whose authority it describes?), registered as its own open
   row rather than left as a paragraph in the row that closed.
3. **Device isolation realism (P1):** `ALET-P1-018` (software boundary is delivered; hardware DMA
   containment remains IOMMU/SMMU-scoped work).
4. **Scheduler/IO robustness (P1):** `ALET-P1-014`, `ALET-P1-019`.
5. **Component security boundaries (P1):** `ALET-P1-021`, `ALET-P1-022`, `ALET-P1-023`,
   `ALET-P1-024`.
6. **Qualification depth (P2):** `ALET-P2-007`, `ALET-P2-008`, `ALET-P2-009`, `ALET-P2-010`.
7. **Networking expansion (P2):** `ALET-P2-020` only after the P1 security-model cluster above is
   closed.
8. **Threat-model/process maturity (P2):** `ALET-P2-029`, `ALET-P2-030`, `ALET-P2-031`.
9. **Architecture governance docs (P3):** `ALET-P3-001`, `ALET-P3-002`, `ALET-P3-003`.

## What Aletheia is

A from-scratch **AI-native operating system** (not a Linux app). Organized around seven primitives —
**Entity → Capability → Context → Intent → Action → Memory → Relationship** — where intelligence is a
native but **untrusted** collaborator, authority is always an explicit **capability** (no ambient
authority), and a deterministic pipeline executes and verifies everything. See PRD-002 / SAD-002.

The v1 premise (Linux-hosted AI app) was rejected by the product owner; the original docs are retained
as `*_v1_superseded.md` for an auditable before/after.

## Latest wave - the OS you can sit in front of, typed at from its own keyboard (2026-08-09, REQ-CON-003, ADR-049)

This one was reported by a user, not found by a gate, and that is the part worth recording.

ADR-044 gave Aletheia an interactive console and ADR-045 made its input interrupt-driven. Both were
gated on all three targets - and both were gated **over the serial line**, because that was the only
input source the console had. Under QEMU with `-serial stdio`, and under the VirtualBox host-pipe
recipe in `docs/VIRTUALBOX.md`, the terminal IS the wire, so a kernel with no keyboard driver is
indistinguishable from one with a working keyboard. `console-e2e.sh` types on the wire. No amount of
running it could have found this.

What found it was someone booting the image on a VirtualBox GUI window: the framebuffer showed the
prompt, the keys reached nothing, and a working OS looked hung.

**What landed.**

* **`kernel_core::keymap` - a keyboard is a second input SOURCE, not a second console.** Scancode set
  1 in, console bytes out, pushed into the SAME `conring` the UART feeds. `shell` keeps one line
  editor, one set of refusals, one overflow policy; two input paths with two editors is how they
  drift. Arch-independent, so all three targets prove it even though only one has the hardware.
* **The decoder's output alphabet is a security boundary.** The line editor refuses bytes it has no
  rule for, so a decoder free to emit arbitrary bytes would be a way to hand it one anyway - from
  hardware someone else may be holding. `feed` emits only printable ASCII, CR, backspace and
  `Ctrl-C`, proved over the ENTIRE input space: all 256 scancodes against every reachable modifier
  state. `Ctrl` with anything but `C` produces nothing rather than one of the other 25 control codes.
* **`kernel-x86_64/src/ps2.rs` - the controller is enumerated, not poked.** The ACPI FADT
  `IAPC_BOOT_ARCH` bit is consulted BEFORE any port is touched, because on a legacy-free platform
  those ports are unclaimed rather than empty and reading them is undefined on the bus. An ABSENT
  field is distinguished from a ZERO one (absent means ACPI 1.0, where the controller is universal).
  Controller self-test and port-1 interface test are separate, because a controller can pass its own
  with a dead port. The configuration byte is rewritten after the self-test that resets it, and then
  **read back** - the translation bit is what makes the set-1 decoder correct, and a controller that
  silently dropped that write would deliver set 2 into a set-1 decoder, presenting as a broken keymap
  rather than a broken assumption. Every wait is spin-bounded: a missing keyboard costs a bounded
  delay and a named reason, never a hang.
* **Proved on every boot, not only interactive ones.** Five bring-up invariants run in the
  non-interactive gate build too, against the real controller, and leave IRQ1 MASKED afterwards -
  arming an input source is the console's decision. A driver that only runs when someone is sitting
  at the machine is a driver no gate covers.
* **`scripts/keyboard-e2e.sh` - the gate the old one could not be.** The serial line is a FILE with
  no writer; the operator drives the emulated i8042 through QMP `send-key`. Every keystroke travels
  controller, IRQ1, PIC, vector 0x21, decoder, ring, line editor - and the assertions are about
  Aletheia's own filesystem changing, not about an echo. Shift is sent as a real held modifier.
* **The ACPI walk left `smp.rs` for `kernel-x86_64/src/acpi.rs`** when the keyboard became its second
  consumer, and now **verifies table checksums**, which the MADT walk did not.
* **`scripts/serial-console.ps1`** - a dependency-free Windows terminal for the VirtualBox serial
  pipe, so the documented recipe needs no PuTTY install.
* **Two defects the sweeps found, in this session's own new code.** The exhaustive scancode sweep
  caught `E0 E0` re-arming the extended prefix instead of resolving it: a device emitting a stream of
  `E0` swallowed every real key after it - a keyboard permanently dead with nothing crashed and no
  error anywhere. And the ACPI extraction found that the MADT walk had never checksummed a table.

**Measured, on this workstation.** `KEYBOARD-E2E: PASS` (7 checks, QEMU+OVMF) - `VM-E2E-X86: PASS` -
`VM-E2E: PASS` (aarch64) - `VM-E2E (riscv64): PASS` - `VM-E2E-VBOX: PASS` - `CONFORMANCE: PASS`
(**104** core behaviors on all three targets) - `CONSOLE-E2E: PASS` - `BUILD-ALL: PASS` -
`QUALITY-GATE: PASS` - `TRACEABILITY: PASS` - `REGISTER: PASS` - `CI-PARITY: PASS`. On x86-64 the
boot reports `device id AB 41` - a translated MF2 keyboard, which is itself the evidence that the
translation bit took. Confirmed by hand on Oracle VirtualBox with `VBoxManage controlvm
keyboardputstring`: the configuration the bug was reported from now types.

**Not claimed.** No USB HID - there is no USB stack, and on machines with legacy USB emulation
disabled a USB keyboard will not work. One US QWERTY layout, no key repeat, no LEDs. aarch64 and
RISC-V still have serial input only, because their QEMU `virt` machines expose no PS/2 controller;
they prove the decoder, not a device. `docs/MATURITY.md` still governs every claim above.

## Previous wave - what "narrower" means, and how long a capability lives (2026-08-09, REQ-CAP-007/008, ADR-048)

Two of the oldest open P1 rows turned out to be the same question asked at two timescales, and both
found live defects rather than merely documenting code that already worked.

`ALET-P1-027` asks what **narrower** means. Every capability guarantee in this repository rests on
attenuation - a delegation may only produce equal-or-narrower authority - and that phrase had no
definition anywhere. `delegate` compared parent and child field by field, inline, with whichever
predicate was at hand; `docs/INVARIANT-CONTRACTS.md` had no section for it. It was the load-bearing
property with the least written down about it.

`ALET-P1-026` asks how long a capability lives. ADR-038 made *entities* durable and said plainly that
authority is not: the engine is born empty at every boot. That is the safe default and a real
limitation - an OS whose authority evaporates on restart has to mint wide authority at a point in the
boot where nothing has authenticated anyone.

They belong in one wave because **the answer to the second is an application of the first**:
persisting authority is the dangerous direction, and what makes it safe is being able to re-run the
admission test on the way back in.

**What landed.**

* **`kernel-core/src/capalg.rs` - the lattice, once.** Three partial orders (action patterns, scopes,
  constraints) and their conjunction `attenuates`, applied by BOTH `spine::CapEngine::delegate` at
  mint time and `capstore::load` at admission time. Two copies of "narrower" is two places for
  authority to widen.
* **A live privilege amplification, found by writing the relation down.** An action pattern denotes a
  SET of actions, and two different questions are asked of patterns: `action_covers(pattern, action)`
  - is this concrete action inside the reach, which is what `evaluate` needs - and
  `action_attenuates(parent, child)` - is the child's REACH a subset of the parent's, which is what
  `delegate` needs. **`delegate` was asking the first question with the child's pattern in the action
  slot.** The two agree on every pattern whose only `*` is trailing and disagree the moment one
  appears elsewhere: `entity.*.*` reaches the string `entity.*` but not `entity.delete`, while
  `entity.*` reaches `entity.delete`. So the delegation was ACCEPTED and the child then authorized an
  action its parent never could - amplification through the one mechanism the model says cannot
  amplify. Fixed in the kernel spine and in the hosted `aletheia/src/capabilities.rs`, because a
  component proved safe on one host and run on the other is not proved safe.
* **`kernel-core/src/capstore.rs` - a persisted registry is untrusted input.** Written as a list of
  refusals, with no partial load, because the parts a partial load drops - the revocation list, a
  parent record - are the parts that make the rest safe. Three of the refusals are substance rather
  than hygiene. **ClockRewound:** expiry is relative to a logical clock, so reloading under a clock
  that restarts at zero un-expires everything; the image carries the clock it was taken under and a
  load under an earlier one is refused. **IdReusable:** ids come from `next_id ^ secret`, so an image
  whose counter could re-mint a stored id would hand a REVOKED token back to whoever still holds it.
  **The cascade is re-derived**, not replayed - naming only a cascade's root, the smallest edit that
  resurrects the most authority, still returns the whole subtree dead.
* **That last check found a real defect in the loader's own first draft.** The re-derivation was
  written as `for r in already_revoked { revoke(r) }`, and `revoke` descends only when its `insert`
  reports the id as newly revoked - every seed was already in the set, so the walk stopped at the
  first node and every descendant came back LIVE. The invariant thins the revocation list rather than
  trusting a well-formed one, which is why it was caught here rather than in the field.
* **Two smaller corrections.** `Entities([])` and `None` are now recognized as the same scope - a
  relation that refuses the narrowest possible delegation on a spelling pushes callers toward wider
  ones. And `Type(T)` vs `Entities([...])` is refused in BOTH directions with the reason recorded: a
  `Target` carries id and etype independently, so neither is a subset of the other, and deciding a
  particular case needs a store lookup an authority check must not acquire (one that reads the store
  can be starved). Deliberately incomplete, never unsound - and the test proves the incompleteness is
  real rather than conservative.

**Measured, on this workstation.** `VM-E2E: PASS` (aarch64) - `VM-E2E (riscv64): PASS` -
`VM-E2E-X86: PASS` (QEMU+OVMF, exit 33 on both boots) - `CONFORMANCE: PASS` (**96** core behaviors on
all three targets, was 88) - `CONSOLE-E2E: PASS` - `BUILD-ALL: PASS` - `QUALITY-GATE: PASS` -
`TRACEABILITY: PASS` (87 requirements) - `REGISTER: PASS` - hosted Core 85 tests and `kernel-core`
295 tests green. Spine invariants 11 -> 13 on every target; a new `[cap] ALL 11 CAPABILITY-LIFETIME
INVARIANTS HOLD` suite on all three, wired into the QEMU and VirtualBox gates. Host side: 10
exhaustive lattice proofs plus a 20 000-chain rejection campaign, and 19 capability-store proofs
including a per-byte per-bit corruption sweep and a per-prefix truncation sweep.

**Not claimed.** The capability image is **checksummed, not authenticated** - the bit sweep proves
the checksum covers every byte, and whoever can write the block can write a matching one; signing it
needs a key whose own lifetime is `ALET-P1-028`, still open. Nothing yet writes the image to a disk:
`save`/`load` are the model and its admission test, proved in kernel space on every target, while
committing it through `persist.rs` is a filesystem ordering decision that gets its own wave. The
logical clock is still a caller-supplied constant on every target - monotonicity is enforced, where a
real monotonic value comes from is not solved. `docs/MATURITY.md` still governs every claim above.

## Previous wave - a second hypervisor, and the assumption it caught (2026-08-09, REQ-QUAL-004, ADR-046/047)

Every boot gate in this repository ran on QEMU. That is a narrower claim than it reads as: a kernel
that boots on QEMU has proved *correct against QEMU*, and no amount of further QEMU testing can find
an assumption the emulator and the kernel hold together. This wave adds the missing rung and, in
doing so, immediately found one.

**The research came first.** `docs/research/RUST-OS-DEEP-RESEARCH.md` surveys the 2026 field -
Redox, Asterinas's framekernel, Theseus, seL4's verification economics, Rust-for-Linux's driver
abstractions, the Kani/Verus/Miri landscape, and the 2026 agent-OS literature (AgenticOS, Agent
libOS) - and ends with what it says about *this* repository: five vindicated decisions and six
exposures. Section 5.2 named the single-hypervisor exposure, and ADR-046 is its answer.

**What landed.**

* **`scripts/vm-e2e-vbox.sh` - the same image, a different hypervisor.** VirtualBox brings its own
  EFI, its own ACPI tables, SATA/AHCI, and **no `isa-debug-exit`**, so the QEMU pass criterion
  (process exit 33) does not exist there. The verdict is the serial log, with **marker parity**
  against the QEMU gate - accepting `[e2e] PASS` alone would pass a kernel that skipped half its
  suites. Four device families VirtualBox cannot emulate (virtio-blk, durable store, the
  cross-reboot persistence proof, network) are listed as **SKIP** and re-named in the summary.
* **`kernel-x86_64/scripts/mkesp.py` - a host-independent image builder.** The macOS builder needs
  `hdiutil`/`diskutil` and the portable one needs `mtools`; neither runs on Windows. This is a
  dependency-free Python GPT + FAT32 ESP writer, deterministic (every GUID derived from the payload,
  no timestamps), producing a byte-identical image on all three hosts. `build-vbox.sh` is now a
  delegator to the gate - two provisioners for one hypervisor is how they drift.
* **The assumption it caught, immediately.** x86-64 virtual-memory invariant 48 asserted that
  `frames::base()` is covered by a **2 MiB block**. Under OVMF the largest conventional region starts
  well above 2 MiB, so it was; under VirtualBox's EFI it starts at `0x100000` - inside the first
  block, which this map splits to 4 KiB pages so VA 0 can have no leaf (ALET-P1-006). The invariant
  had conflated a *security* property (bulk RAM is RW+NX) with a *structural* one (it gets huge
  pages) and encoded "the firmware's memory map looks like QEMU's". Now two invariants, and the
  structural one probes an address proved to be outside every split span.
* **ADR-047 - the Service boundary stops importing POSIX.** `service.rs` imported
  `std::os::unix::net::{UnixListener, UnixStream}` at the one seam the SAD says is Aletheia-owned, so
  the hosted Core did not compile on Windows at all (`E0433: cannot find 'unix' in 'os'`). There is
  now an `aletheia/src/transport.rs` seam with per-host backends: a Unix socket, or on Windows a
  loopback listener behind a **rendezvous file carrying a 32-byte connect token** compared in
  constant time. The weaker posture of the Windows backend is written down, not glossed.
* **ALET-P1-009 and ALET-P1-011 closed - ring-3 invariants 27 to 34.** The static half of P1-009
  pinned the `TrapFrame`'s offsets; a save/restore pair that swapped two registers consistently would
  satisfy every one of those asserts. The **fuzz half** primes 15 distinct sentinels, traps, has the
  kernel compare the whole saved file, **resumes the same frame**, and traps again - so the restore
  direction is proved, not assumed. P1-011 stops testing only the fault *classifier*: a ring-3 `ud2`
  and a ring-3 `hlt` now take **vector 6 and vector 13** for real, are classified as user faults,
  and are contained by the supervisor - with both vectors handed straight back to their fatal
  catch-all handlers afterwards, because a safety net taken down for a test has to go back up.
* **ALET-P2-001 closed - the dated toolchain pin.** `nightly-2026-08-09` (rustc 1.99.0-nightly,
  `771916f90`). The previous attempt was reverted because the install rolled back; the same failure
  reproduced here as a *partial* install leaving a named-but-unusable toolchain, and
  `docs/TOOLCHAIN.md` now records that trap. Every nightly-installing job in both pipelines names the
  date, not just `quality`.
* **`.gitattributes` - LF for executable text.** With Git's Windows default (`core.autocrlf=true`)
  every gate script checked out with CRLF and died looking for an interpreter named `bash\r` before
  running a single check - in WSL, in containers, anywhere the clone was shared. A qualification
  story that is host-independent cannot rest on the developer's global Git config.
* **`qemu-system-riscv`.** On current Ubuntu the riscv64 emulator left `qemu-system-misc`; both
  pipelines installed the wrong package, and the gate died inside its perl watchdog with
  `Died at -e line 1.` and 21 marker-missing lines - which reads as a broken kernel. Both pipelines
  fixed, and the aarch64/RISC-V gates now **preflight the emulator by name** before building.
* **Dependencies at latest.** `sha2` 0.11, `chacha20poly1305` 0.11, `ed25519-dalek` 3, `rand` 0.10,
  `ulid` 3, plus every lockfile refreshed. The RustCrypto bump deprecated `Array::from_slice`; the
  AEAD nonce is now a checked conversion, which is the case the deprecation exists for.
* **`docs/VIRTUALBOX.md`** - how to build the image and run Aletheia in Oracle VirtualBox yourself,
  headless or in the GUI, including the interactive-console build over a host serial pipe.

**Measured, on this workstation.** `VM-E2E-VBOX: PASS` (VirtualBox 7.2.6, EFI, 4 vCPU, 512 MiB, 13
required markers, 4 SKIP) - `VM-E2E-X86: PASS` (QEMU+OVMF, exit 33 on both boots) - `VM-E2E: PASS`
(aarch64) - `VM-E2E (riscv64): PASS` - `CONFORMANCE: PASS` (88 core behaviors on all three targets)
- `CONSOLE-E2E: PASS` - `BUILD-ALL: PASS` - `TRACEABILITY: PASS` (85 requirements) -
`CI-PARITY: PASS` - hosted Core 83 tests and `kernel-core` 266 tests green on **Linux and Windows**.

**One difference worth knowing:** VirtualBox does not expose SMEP, so the boot prints
`exec protections incomplete (NX=true, SMEP=false) - W^X degraded on this CPU`. NX is present, the
live W^X audit still reports **0 violations**, and the kernel reports the degradation rather than
assuming a CPU feature it could not verify.

**Not claimed.** VirtualBox is a second *emulation* of the platform contract, not the contract -
ADR-013's hardware rung is untouched. `docs/MATURITY.md` still governs every claim above.

## Previous wave — the console stops spinning (2026-08-07, REQ-CON-002, ADR-045)

Every driver here polls, and `docs/MATURITY.md` lists that as item 3 of what production would
additionally require. The console made it concrete: `run_loop` spun on `getc`, reading an empty
register almost every time, and burned a core doing it.

- **`kernel-core/src/conring.rs` — a bounded ring, and one real decision.** The overflow policy is
  **DROP-NEWEST**. A ring that overwrites its oldest byte is the conventional choice and it silently
  changes MEANING: `rm notes` with its head overwritten reads as `notes`, a different command the
  editor would accept without complaint. Dropping the newest truncates a burst instead — the operator
  sees a short line and retypes it, and nothing already typed was rewritten underneath them. Capacity
  equals `MAX_LINE`, so one whole line always fits; an overflow means the operator got ahead of a
  running command, never that a line was too long for the buffer carrying it. **Every dropped byte is
  counted and `mem` reports it**, because input loss the operator cannot see is loss they will blame
  on the command they typed.
- **All three targets take the interrupt.** On aarch64, vector 0x280 — an interrupt while the KERNEL runs — had been a
  fatal catch-all; it is now a handler that is **still fatal for every INTID except the console's**,
  because making a fatal vector live must not quietly swallow what nobody expected. GICv2 routes
  PL011's SPI 1 (INTID 33) at a priority BELOW the timer's, so a keystroke never outranks preemption.
- **x86-64 and RISC-V each needed something aarch64 did not.** x86: arming the console **masks IRQ0
  first** — the boot leaves the PIT free-running for the ring-3 suite, so the instant `sti` executed
  the timer fired thousands of times a second into a handler the console has no use for and the
  session never progressed. RISC-V: a **PLIC driver, which did not exist**, whose context number is
  DERIVED rather than constant — QEMU lays contexts out as `2N`/`2N+1` per hart and OpenSBI's boot-hart
  lottery may hand us any hartid, so a hardcoded context configured hart 0 whichever hart was really
  running and the console worked or went deaf on a coin flip inside the firmware.
- **Three bugs the gate caught, all worth naming.** (1) Acknowledging the UART *after* draining loses a
  byte that lands mid-drain — its condition is cleared while the byte still sits in the FIFO, so no
  further interrupt is raised and the console goes deaf. It presented as a session that answered six
  commands and ignored the seventh. Clear BEFORE draining. (2) The receive interrupt alone never fires
  for a burst shorter than the FIFO trigger level, which is exactly what a human typing one character
  at a time produces; the receive-TIMEOUT interrupt is what makes single keystrokes work. (3) The gate
  itself sampled its prompt count AFTER typing — if the guest answered before the sample, that command
  burned its full 30s timeout and every later one inherited the skew, which is exactly the watchdog.
- **No lock.** Two parties — handler (producer) and loop (consumer). A spinlock taken in a handler and
  in the code it interrupts is the classic self-deadlock; the consumer masks IRQs around `pop`, and the
  handler cannot be re-entered because the CPU masks IRQs on entry and nothing unmasks before `eret`.
- **Proof.** 8 live ring invariants on **every** target, 9 host tests that attack the surviving
  contents rather than the counters (after any overpressure, what remains must be exactly the oldest
  prefix; a typed command must come back intact behind a flood), and **3 behaviors added to the
  conformance contract (85 -> 88)** so the overflow policy cannot diverge by CPU — plus the
  three-target scripted-operator gate, now driving three real interrupt controllers.

**Not claimed.** The **transmit** side is still polled on every target. Each handler's wire path is
hardware and no host test covers it — what proves it is the scripted-operator gate, and the three bugs
above are the evidence that gate has teeth. Framing/parity errors are read past rather than reported.
And `run_loop` still spins when the ring is empty; a `wfi` there is the obvious next step, not claimed here.

## Previous wave — an OS you can sit in front of (2026-08-07, REQ-CON-001, ADR-044)

Every gate here boots, proves its invariants and **exits with a verdict**. That is what makes the claims
checkable, and it is also why the most ordinary question about an operating system had no answer: *can I
run it?* You could run a proof. You could not run the system — nothing kept the machine up, and all three
UART drivers were transmit-only.

- **`kernel-core/src/shell.rs` — the console, defined once.** A serial port differs per target; a line does
  not. Each target supplies only a non-blocking `getc` and a way to print, and inherits the editor, the
  command grammar, every refusal and the loop. Three consoles would have meant three input boundaries with
  three sets of bugs — exactly the divergence `conformance.sh` exists to catch.
- **The editor is a filter, not a buffer.** Only printable ASCII may ENTER a line; only CR/LF ends one;
  Ctrl-C discards it; backspace/DEL and Ctrl-U edit it; **every other byte is dropped without an echo**, so
  an escape sequence, a mouse report or a pasted binary cannot become a command argument. A line stops
  growing at 256 bytes — a terminal that pastes a megabyte cannot make the kernel allocate one. Because only
  ASCII is admitted, the buffer is valid UTF-8 by construction rather than by a check that could be skipped.
- **Commands drive only what is already proved** — the named-object namespace over the journal, the frame
  allocator, the HAL clock: `help`, `arch`, `uptime`, `mem`, `df`, `ls`, `stat`, `cat`, `write`, `rm`,
  `echo`, `halt`. `write` goes through `Filesystem::replace`, so a keystroke sequence is ONE transaction and
  a crash mid-write leaves the old contents or the new ones, never a vanished name.
- **Interactivity is a cargo feature, off by default.** Without `--features interactive` the boot ends
  exactly as before, so every gate keeps its exit-code contract. With it, the boot hands the machine to the
  serial line after the suites pass.
- **Proof.** 15 live invariants **per target** — scripted sessions against a real namespace, run inside the
  boot gate, so the gate covers the code an interactive boot runs rather than a parallel path only humans
  see. Plus 20 host tests that attack it: all 256 byte values swept against the editor, a paste 100x the
  line bound, verbs matched exactly rather than by prefix, an oversized write refused rather than truncated,
  and non-text contents reported as a byte count instead of sprayed at a terminal. **7 behaviors joined the
  cross-architecture conformance contract (78 -> 85)**, because what may become a command, and whether a
  console write is committed, must not vary by CPU.
- **`scripts/console-e2e.sh` answers the original question, on all three targets.** A scripted operator
  waits for the prompt, types, writes an object, reads it back and halts — then the machine **boots again**
  and still holds what was typed. `halt` exits through the same path the gates use, so a session keeps an
  exit-code contract and a wedged console fails as a timeout instead of hanging CI.
- **Waiting, not sleeping.** The operator watches for `aletheia> ` before typing. A byte typed too early is
  not merely early: on x86-64 the boot's `serial::init` CLEARS the receive FIFO, so those keystrokes are
  destroyed. A fixed delay would have made the gate a race against however long the suites take on that host.
- **`scripts/run-interactive.sh [aarch64|riscv64|x86_64]`** is the human entry point: build, boot, get a
  prompt, keep the disk between runs.

**Not claimed.** The dispatcher runs in **kernel space** over the kernel's own objects — it is not a
user-mode shell process over a syscall ABI, and the syscall surface each target exposes today is narrower
than one would need. The console is polled, like every driver here. `getc` reads a hardware register, so it
is the one part no host test can prove; the sweeps prove what happens to a byte after it arrives, and the
scripted-operator gate proves real bytes do arrive.

## Previous wave — addresses that must be dead in EVERY space (2026-08-07, GAPS4 ALET-P2-033)

VA 0 and the ring-0 stack guard are given no descriptor on purpose, and ALET-P1-006/012 proved both — of
the map each kernel built **for itself**. A per-process root is a different tree, so the property was a
claim about one space in a system that makes many.

- `kernel-core/src/deadva.rs` states the rule once (contract §INV-DEADVA): a target DECLARES its dead
  spans, `audit` walks them in **any** root, and it asks two questions rather than one — the page must not
  translate, **and** no descriptor at any level may still cover it, because an unreachable page under a live
  2 MiB block is one split away from being alive again. An **empty declaration fails**: a target that
  forgets to declare has proved nothing, and must not look like one with no dead pages.
- Wired **fail-closed** into all three space builders — `build_space` (x86-64), `build_identity`
  (aarch64/RISC-V). A tree that fails the audit yields **no space at all** and returns its frames; handing
  out a space that can reach the guard is worse than failing to build one.
- **It found the defect the register row only suspected.** `build_space` copied whatever CR3 held, and
  `kmap::activate()` runs *after* the virtual-memory suite — so a space built during it copied OVMF's tree,
  which maps VA 0 as RAM and covers the ring-0 stack guard with a 2 MiB huge page. Ring 3 could reach two
  addresses the kernel's own map deliberately cannot: the guard **inverted**. `vm::space_source_root()` now
  derives from the kernel's own map whenever one exists, so the property holds by construction rather than
  by activation order.
- **Two neighbouring invariants had encoded the old source** and were corrected rather than left passing for
  the wrong reason: the teardown suite searched PML4 slots 1..512 for a shared kernel table, which matched
  only because OVMF's tree spread across them. The kernel's own map covers 4 GiB entirely inside PML4[0], so
  the sharing a teardown must not disturb lives one level down, in the private PDPT.
- **Gates:** virtual-memory invariants 62 → **66** (aarch64, RISC-V) and 66 → **71** (x86-64); the
  `conformance.sh` core contract 74 → **78** named behaviors, PASS on all three targets; six host tests;
  `quality-gate` PASS, `register` PASS, `traceability` PASS, `ci-parity` PASS.
- **Register: 31 → 32 resolved, 31 → 30 open.**
- **Also this wave:** `docs/gap/TRIAGE.md` (an external audit dated 2026-08-06) was committed **with a
  verification note** rather than acted on. Its CRITICAL RISK-001 — `[FAIL 11] fs: two objects never share
  a data block` — **does not reproduce**: all 15 filesystem invariants pass on every target, including over
  the real virtio-blk device. An untracked document asserting a false release blocker is exactly the
  manual-metric drift the register exists to kill, so the correction lives at the top of the file.

## Wave — the task lifecycle, written down and attacked (2026-08-03, GAPS4 ALET-P1-015)

Four task states, and nothing said which transitions are impossible. A lifecycle bug is usually a state
that is only *briefly* wrong, so `docs/INVARIANT-CONTRACTS.md` §INV-TASK states five invariants and every
test drives long sequences, checking its property after **every** event.

- `Finished` is **terminal** under every following event (swept with all four states as interference); a
  Blocked task is never dispatched and only `unblock` makes it eligible; at most **one** task is Running and
  `current()` names exactly it, re-checked after each of 500 random events; `runnable_len` equals the
  rotation — Ready **plus** the Running task, which rotates to the tail — and never counts Blocked or
  Finished; and an event naming a task that was never spawned changes **nothing**.
- **The last one found a real defect.** `block`/`finish` INVENTED a task in the state table from a stray id,
  so the scheduler could believe in a task it never created and eventually try to resume a context that does
  not exist. Fixed in `sched.rs` in the same commit.
- **And one invariant corrected the contract rather than the code:** `runnable_len` counts the rotation, so
  the first draft (Ready-only) was wrong about what the scheduler promises — worth recording, because a test
  written to the wrong definition is how a correct implementation gets "fixed" into a broken one.
- **Register: 29 → 31 resolved, 33 → 31 open** — the same wave also closed ALET-P1-020 with
  §INV-STORE-ERR: the error kinds are distinguishable, a device error is surfaced (including a failed
  **flush**, the durability barrier — swallowing it would report durability that does not exist), the
  filesystem preserves the cause rather than flattening it, and every refusal is proven a no-op by
  comparing the whole device image byte-for-byte before and after.

Gates after the wave: `build-all` PASS (22 host test binaries), `register` PASS, `traceability` PASS (79
requirements).

## Previous wave — what a device is allowed to touch (2026-08-03, GAPS4 ALET-P1-018, REQ-DRV-006)

Every driver here hands a device a **raw physical address** and trusts it to write only there, and nothing
checked the address. Enabling PCI bus-master (ADR-037) made that concrete rather than theoretical: a
descriptor with a wrong address is a device writing wherever the number points — kernel text, another task's
frame, a page table — and the memory model sees none of it, because none of its checks sit on the path where
a number becomes a descriptor.

- **`kernel_core::dma` is the boundary the kernel can enforce without an IOMMU.** A driver registers what it
  intends a device to reach, naming itself owner; registration refuses a misaligned or null address and
  anything **overlapping the kernel image** — a device writing into kernel text is the write-to-code path
  W^X closes, arriving from the other side.
- **Deny by default**, and a range that extends past its registration is refused too: partial visibility is
  not visibility. **One frame, one owner** — two drivers pointing one device at one frame is a bug in the
  same way a double free is. **Revocation ends visibility** and revoking twice is refused, so a frame
  returning to the allocator stops being something a device may be told about (the DMA twin of ADR-033).
- **An undeclared image span is visibly unenforceable, not silently permissive.** `image_declared()` is
  false until a target declares its span and the boot invariant checks *that*, so a target which forgets
  fails a check rather than losing the rule quietly. Every refusal is counted, so a boot can report the
  boundary did work.
- **Nine invariants on all three targets** (`ALL 9 DMA-BOUNDARY INVARIANTS HOLD`, boot failing `240 + i`),
  three of them in the conformance contract (69 → **72**): what the kernel may tell a device is policy, not
  a hardware property.
- **And it became a GATE, not just a policy.** `Virtqueue` owns a registry: its ring frame is registered at
  setup, a buffer must be registered before it can be named, and **`add` refuses an address that is not
  visible** — the check sits exactly where a number becomes a descriptor, the only place a wrong one could
  escape. virtio-net registers its receive and transmit buffers, and a new invariant on all three targets
  proves the gate **denies by default**: an address far from any buffer is refused, and so is one that
  overruns a registered buffer (network invariants 4 → 5, conformance 72 → **73**).
- **`virtioblk` is gated too.** It predates `virtq` and keeps its own fixed ring, so it would have been the
  one path still naming unregistered addresses: its ring and data frames are registered at init, and
  `request` checks the header, status byte and data buffer **before** any becomes a descriptor. Its suite
  takes the gate's answer from the **driver** rather than assuming it — like the geometry check — so a device
  whose gate stopped working fails instead of the suite passing on a default (virtio-blk invariants 20 → 21,
  conformance 73 → **74**).
- **ALET-P1-018 stays open for the part software cannot do:** nothing stops a device that *invents* its own
  addresses. That needs an IOMMU/SMMU, and the row says so rather than implying coverage.

Gates after the wave: `quality-gate` PASS, `build-all` PASS, `e2e-all` PASS, `conformance` PASS (72 × 3),
`register` PASS, `ci-parity` PASS, `traceability` PASS (78 requirements).

## Previous wave — kill the task, keep the system (2026-08-03, REQ-REL-002)

ADR-039 gave every fault a verdict, and `KillTask` had **nowhere to go**: each target's handler ended the
boot, because nothing could remove one task and let the rest continue. That is why `docs/MATURITY.md` listed
a task supervisor first among the things production would additionally require — without it the kernel
*detects* a bad access rather than *surviving* one, and every user bug is a system outage.

- **The mechanism was closer than it looked.** On x86-64 `isr_pf_entry` already abandons the faulting task
  and returns to the scheduler — but only for an *armed* fault the isolation trial declared in advance;
  anything else was fatal. The missing piece was policy, not assembly.
- **`kernel_core::supervisor`** turns a verdict into an action: a **user** fault terminates the task; a
  kernel fault, corrupt translation or unknown report **escalates**, because the kernel cannot sensibly kill
  a task for its own bad access. A `KillTask` verdict with **no task to blame** escalates too — that is a
  kernel bug wearing a user fault's clothes.
- **A terminated task is terminated forever, with a reason.** `may_run` is what a scheduler asks before
  dispatch and never answers yes again; the reason distinguishes a fault from an exit from a policy kill, and
  termination is idempotent keeping the **first** reason — a later sweep must not overwrite the fault that
  actually killed it. Contained and escalated faults are counted separately, because a system that quietly
  turned kernel bugs into task deaths would look healthier than it is.
- **The live proof is a fault taken on purpose.** A ring-3 task reads a supervisor-only page it never
  declared. Four invariants require: exactly one task terminated; the dead task may never run again and its
  recorded reason is the fault; **zero** escalations; and — the point — **a later ring-3 task still runs and
  proves its own invariant**. The boot log now reads
  `task 7 TERMINATED (Fault(UserNotMapped)); system continues`. Ring-3 invariants 22 → **26**.
- **Not claimed:** the supervisor does not free the dead task's address space (that is
  `teardown::destroy_address_space`, and doing it on a trap stack belongs in a scheduler reap step rather
  than being claimed here), does not restart anything (REQ-REL-001 needs a supervision tree, still
  architecture only), has no quotas or rate limits, and — after the same wave — the handler routes
  through the supervisor on **all three** targets, each asserting the policy behaves (a user fault
  terminates that task, a kernel fault escalates; conformance 68 → **69** behaviors). What is x86-64-only is
  the **end-to-end** proof: taking an undeclared fault and then running another task. aarch64 and RISC-V need
  their own unarmed excursion, and their invariants say exactly what they prove and no more.

Gates after the wave: `quality-gate` PASS, `build-all` PASS, `e2e-all` PASS, `conformance` PASS (68 × 3),
`register` PASS, `ci-parity` PASS, `traceability` PASS (77 requirements).

## Previous wave — the network answers back (2026-08-03, GAPS4 ALET-P2-020, REQ-NET-001/002)

Networking was the largest remaining "an operating system does this" hole: architecture text and nothing
else. It could not reuse the block driver, for a precise reason — a block device has ONE queue with one
request in flight, and `virtioblk` encodes exactly that in a fixed layout. A NIC needs two queues, and its
receive queue must have buffers **posted before the device is allowed to run**, because a packet arrives
whether or not the driver is ready and a queue with no buffer simply drops it.

- **`kernel_core::virtq` — the ring mechanics, reusable.** A `Virtqueue` per queue index over the existing
  CPU and bus seams: `add` / `kick` / `poll_used`. `virtioblk` keeps its own proven single-queue path
  untouched. `last_used` lives in the queue, because a driver that forgets how far it consumed the used ring
  handles one packet twice — or reuses a transmit buffer still in flight.
- **`kernel_core::virtionet` — the driver, and the smallest honest stack.** Feature negotiation takes only
  MAC (so the address comes from the device rather than being invented) plus `VERSION_1`; every offload is
  declined, which is what keeps the header all-zero. Receive buffers are posted before `DRIVER_OK` and
  re-posted before each frame is returned — both orderings load-bearing.
- **The proof is that something ANSWERS.** A transmit-only driver is indistinguishable from a frame that
  vanished. So the suite talks to QEMU's gateway: **ARP** must answer "who has 10.0.2.2?" with that address
  and a MAC; then an **ICMP echo** with two correct checksums (the IP header's and the message's) must come
  back with matching identifier, sequence and payload — a wrong checksum is dropped by the peer in silence,
  so the reply arriving proves the packet was well *formed*, not merely well intentioned. A **second** echo
  must match on its own sequence, proving the driver reads the reply instead of assuming the next frame is it.
- **All three targets, both buses, first boot on two of them.** aarch64 and RISC-V over virtio-mmio, x86-64
  over virtio-pci: `ALL 4 NETWORK INVARIANTS HOLD`, boot failing `220 + i`. The four behaviors joined the
  conformance contract (64 → **68**), so "the network works" cannot vary by CPU — or by bus.
- **x86-64 failed first, and it was a real seam bug**, not a wiring slip: `PciTransport::identity()`
  returned the *PCI* device id (`0x1041`) where the shared driver expects the *virtio* device kind, so a
  perfectly good NIC was refused. A seam whose meaning differs per bus is not a seam.
- **Not claimed, and it is a lot:** no TCP, UDP, DHCP, routing, fragmentation, ARP cache or socket layer;
  the address is the fixed `10.0.2.15` QEMU expects and every reply is matched synchronously by the code
  that sent the request. Frames that are not the answer are **counted** (`dropped()`), not queued, so a
  failing wait beside a nonzero count distinguishes "the peer said nothing" from "the driver threw the
  answer away". **ALET-P2-020 moves from `deferred` to `open`** — real gated code exists, and what remains
  is named in its row.

- **And the register now checks itself** (`scripts/check-register.sh`, a `register` job in both pipelines,
  ALET-P2-012): a `resolved` row citing a file that no longer exists fails CI, so does a row citing a `REQ-`
  id the matrix does not track — and so does **the rollup arithmetic disagreeing with the rows**, which is
  the one drift that would quietly invalidate every count this project reports. Wiring it found a real
  defect immediately: one row cited a generic `.cargo/config.toml` that exists per crate but not at that
  path. With `check-traceability.sh` and `check-ci-parity.sh`, all three claim surfaces are machine-checked.
  x86-64 also now asserts the map builder recorded exactly two deliberately-unmapped pages (ALET-P2-034),
  closing the other half of the guard proof: absence was proved by walking the tree, intent was not.

Gates after the wave: `quality-gate` PASS, `build-all` PASS, `e2e-all` PASS (each target booted twice, now
with a NIC attached), `conformance` PASS (68 core behaviors × 3 targets), `register` PASS, `ci-parity` PASS,
`traceability` PASS (75 requirements).

## Previous wave — a layout you can check, and two addresses that must never translate (2026-08-03, GAPS4 ALET-P1-006/012)

Each target knew its address-space layout as scattered literals — a RAM base in `frames`, a peripheral
window in `vm`, a user VA in `usermode`, a stack in `linker.ld`. Nothing stated what the layout *was*, so
nothing could check the properties a layout must have. Two of them were quietly false.

- **Stacks had no guard.** Every kernel stack grows down into whatever the linker put below it — `.bss` on
  the QEMU targets, neighbouring statics on x86-64. An overflow did not fault; it corrupted state silently
  and surfaced later as impossible behavior. Now `linker.ld` reserves `__stack_guard` below
  `__stack_bottom` (aarch64/RISC-V), and on x86-64 the guard is the first page of a page-aligned `KSTACK`
  with `RSP0` moved above it. Each identity map **splits the containing block** — a 2 MiB block cannot have
  a hole — and leaves that page with no descriptor at all.
- **And VA 0 was mapped.** `vmaddr` has refused the null page at the mapping APIs since ADR-029, but every
  target's *boot identity map* covered page 0 anyway: inside the peripheral device window on aarch64 and
  RISC-V, as ordinary RAM on x86-64. A kernel null dereference therefore read — or **wrote** — a real MMIO
  register or real memory instead of faulting. **Writing the layout check is what found it.** Punched out
  on all three targets (RISC-V needs two levels of split, since a gigapage cannot have a hole either).
- **`kernel_core::layout` — the layout is a declaration you can check.** A target declares named regions
  with their privilege; `validate` refuses overlap, misalignment, a region containing the null page, and a
  user region that merely **abuts** a kernel one (something that grows would cross the boundary without
  ever being unmapped). Every boot suite runs the validation.
- **Proved on the live tree.** Four `guard:` and three `layout:` invariants per target — no translation, no
  leaf at any level, the stack's own pages still mapped (a guard that cost the kernel its stack would be
  worse than none), the stack still W^X, VA 0 dead. Virtual-memory invariants 55 → 62 (aarch64/RISC-V) and
  58 → 65 (x86-64); the guard-page and null-page behaviors joined `conformance.sh` (62 → **64** core
  behaviors), because "a null dereference faults" must not depend on the CPU.
- **The first version of the guard invariant was wrong, and the failure taught something.** It asserted
  against `active_root()`, but by that point in the suite CR3 may hold a per-process space an earlier test
  built — so it must assert against the **kernel's own** map (`kmap::root()`).
- **KASLR: none, deliberately, with the reason recorded** rather than the absence implied. Every target
  identity-maps, which is what keeps DMA auditable (a driver hands the device the address it writes
  through), so randomizing the kernel base is a different memory model, not a flag — and KASLR defends
  against reading a pointer and using it, whereas every effect here is capability-gated. What it would take
  is written down: a higher-half split, an offset-mapped physical window, PIE images.
- **Not claimed:** no guards around the heap or the SMP secondaries' stacks, and per-process spaces built
  by copying the live tree get their own mappings — re-asserting both properties inside every derived space
  is a named follow-on. **Register: 24 → 26 resolved, 35 → 33 open.**

Gates after the wave: `quality-gate` PASS, `build-all` PASS, `e2e-all` PASS (each target booted twice),
`conformance` PASS (64 core behaviors × 3 targets), `ci-parity` PASS, `traceability` PASS (73 requirements).

## Previous wave — the source itself gets a gate (2026-08-03, GAPS4 ALET-P2-003/004/005)

The boot gates prove the OS behaves. Nothing proved the **source** was in the state the project claims:
formatting drifted, `clippy` was never run in CI, no advisory scan existed, no dependency's license had
ever been checked, and the bare-metal crates rode an **unpinned `nightly`** — a compiler that changes
under the build without a commit.

- **`scripts/quality-gate.sh`** — one script, run by BOTH pipelines (a new job `quality` in
  `.github/workflows/ci.yml` and `.gitlab-ci.yml`, which `check-ci-parity.sh` requires to match): `cargo
  fmt --check` and `clippy -D warnings` for every crate, `cargo audit` against the committed lockfiles,
  a license allow-list, and an SBOM.
- **Skips are LOUD.** A check whose tool is missing prints an explicit `SKIP` line and is named again in
  the summary, so a green run means "everything that could run, ran" — the same doctrine the VM gates use
  for an absent disk. CI installs `cargo-audit`, so the advisory step runs there rather than skipping.
- **`scripts/sbom.py`** writes `build/sbom/<crate>.json`: every dependency's name, version, source and
  license, sorted, **with no timestamp** — a timestamp would make every run a diff and hide the real ones.
  Both pipelines publish it as an artifact. 117 packages inventoried.
- **The dated toolchain pin was attempted and deliberately NOT claimed.** It was written, then reverted:
  installing `nightly-2026-07-20` failed on this workstation (rustup rolled the install back), so every
  gate would have run against a different compiler than the files named — an unverified claim, which is
  what the register exists to prevent. **ALET-P2-001 stays open**, with the reason in its row and the
  finishing steps in `docs/TOOLCHAIN.md`. What did land and is verified: every toolchain file now requests
  `clippy`/`rustfmt` explicitly (so the gate cannot skip for a missing component), and the host crates name
  `stable` — the fix for a failure this repo has actually had, since without a named channel `cargo`
  resolves to whatever is first on PATH, and on macOS that is often Homebrew's, which ignores
  `rust-toolchain.toml` and builds for the host triple.
- **Writing the gate found four real defects in this session's own code**: dead code in the new
  `pci.rs` (a `bdf` field and accessor nothing read), `.clone()` on a `Copy` capability token, mutable
  borrows where a shared one was enough, and two crates whose formatting had drifted. All fixed. A lint
  gate that finds nothing on first run is usually a lint gate that is not running.
- **The parity gate caught the wave itself**: `check-ci-parity.sh` failed because CI executed
  `scripts/quality-gate.sh` while STATUS.md did not mention it — exactly the drift it exists to prevent.
- **Register: 21 → 24 resolved, 38 → 35 open** (three rows closed; ALET-P2-001 left open on purpose).

Gates after the wave: `quality-gate` PASS (advisories SKIP on this workstation, run in CI), `build-all`
PASS, `e2e-all` PASS, `conformance` PASS (62 core behaviors × 3 targets), `ci-parity` PASS,
`traceability` PASS (71 requirements).

## Previous wave — a fault handler must know what happened, and must not re-enter itself (2026-08-03, GAPS4 ALET-P1-009/010/011/013)

Three gaps sat on the trap-entry path with the same shape: the code worked, and there was no *model*
behind it. Handlers printed the raw architectural code and exited — nowhere to state that a fault
reporting a **reserved bit** in a translation structure must never be resumed. A handler runs on top of
whatever it interrupted, and nothing said what happens if it re-enters. And the x86-64 trap assembly
addresses its frame with literal byte offsets that only five compile-time asserts covered.

- **`kernel_core::faultclass` — one total, fail-closed model (REQ-FAULT-001, ADR-039).** A normalized
  `Fault` (present/write/user/exec/reserved/from-kernel) that all three architectures decode into
  (x86 error code; aarch64 EC + DFSC class + WnR; RISC-V `scause`), a `FaultKind` for what it *means*,
  and a `FaultVerdict` for what the kernel may do. Only user faults are survivable; a reserved bit
  dominates every other reading, because if a translation structure is malformed then what the other
  bits "mean" is not knowable.
- **Unknown degrades to fatal, which is what makes the model safe to extend.** An architectural bit the
  model does not interpret (protection key, shadow stack, SGX) makes the fault `Unknown` — never
  classified from the bits that happen to be understood. Even `Fault::none()` is a kernel fault, so a
  decoder that forgets a field cannot make one look routine.
- **RISC-V's asymmetry is stated, not papered over.** `scause` reports neither present-vs-absent nor the
  faulting privilege, so those are parameters from the caller. Inventing a bit the ISA does not report
  would make the classification a guess that reads like a fact.
- **`kernel_core::reentry` — re-entry becomes detectable and fatal (REQ-FAULT-002).** The x86-64
  fault-report path is guarded: a fault inside fault reporting prints one line and exits 106 instead of
  recursing until the stack runs out and the machine triple-faults. The guard is a compare-exchange, so
  it also catches a *second CPU* entering a section with no lock — a different bug, same consequence —
  and refusals are counted, so a caller that swallows one still leaves evidence.
- **The manual ABI now fails the BUILD.** The `TrapFrame` assert block pins size, alignment, the register
  array's offset and width, the named register indices, every `iretq`-frame offset the assembly's literals
  use, and that nothing hides past the last field.
- **Proved where it runs.** Exhaustive host sweeps — every x86 error code including unknown-bit
  combinations, every EC/DFSC pair, every `scause`, asserted over the whole input space rather than
  sampled — plus x86-64 boot invariants 56–58 (virtual-memory 55 → 58) proving the classification and the
  guard behave inside the kernel. A classification that only holds in `cargo test` protects nothing.
- **Two rows closed, two REFINED rather than flipped.** ALET-P1-010 and ALET-P1-013 are resolved.
  ALET-P1-009 keeps its `fuzz` half (a register-file round-trip through the real trap assembly) and
  ALET-P1-011 keeps adversarial *entry* testing (real ring-3 `#UD`/`#GP` trials, contained) — each with
  the remaining work named in its register row instead of being closed on a partial claim. Register:
  19 → 21 resolved, 40 → 38 open.
- **Wiring scope, stated:** the classifier and guard are live on x86-64; the aarch64 and RISC-V decoders
  are host-proved but not yet wired into those handlers.

Gates after the wave: `build-all` PASS (21 host test binaries), `e2e-all` PASS (each target booted
twice), `conformance` PASS (62 core behaviors × 3 targets), `ci-parity` PASS, `traceability` PASS (70
requirements).

## Previous wave — what must never happen, written down (2026-08-03, GAPS4 ALET-P1-005 / P1-016 / P1-017 / P1-025)

Four subsystems were delivered as *code that works* with no written statement of **what must never
happen**: cross-core TLB shootdown, priority inheritance, IPC cancellation, capability revocation. A
passing test tells you the case it runs; a contract tells you the cases that must never pass. This wave
writes the contracts and then attacks them.

- **`docs/INVARIANT-CONTRACTS.md` — 25 numbered invariants** across the four clusters. Each is stated as
  something that must hold (not as a description of the code), carries the reason it is load-bearing, and
  names the host test that adversarially attempts to violate it. An id without a proof is a documented bug.
- **INV-TLB (6).** The requester is about to reclaim the frame the stale mappings point at, so: `request`
  completes only if EVERY addressed target acknowledged; an ack never precedes the invalidation it covers
  (checked from *inside* `perform` — the counter must not have moved yet); exactly-once performance, no
  drops or duplicates; no borrowing another requester's acks; an aborted wait is `false` and never a
  partial success; a bogus target id is ignored rather than waited on.
- **INV-PRIO (7).** A holder is never weaker than anyone blocked on an endpoint it holds — re-checked
  after **every** operation against a tracked waiter model rather than once at the end; donation is
  transitive along a whole A→B→C chain; donation **ends** at release; donation never manufactures priority
  above the highest base, over a dense tangle of holds and waits; the scheduler never dispatches a Blocked
  task nor a weaker Ready one over a stronger one; a donation **cycle** (deadlock) terminates instead of
  recursing; an unauthorized `acquire`/`wait` changes nothing — otherwise any task could force a donation.
- **INV-IPC-CANCEL (6).** A cancelled message is never delivered by any later receive; cancelling
  something already gone returns `false` and changes nothing (a `true` return is the sender's evidence it
  won the race, so a lie there is worse than a refusal); exactly the named message is removed and order
  preserved; every message reaches **exactly one** terminal trace event; a cancelled slot frees one unit
  of bounded capacity without lifting the bound; a deadline and a cancel never both claim one message.
- **INV-CAP-REVOKE (6).** After `revoke` returns, 50 further attempts all deny and no effect runs;
  revocation is permanent — a revoked parent cannot be delegated from, a fresh mint never reuses the id,
  and offering a revoked token beside a live one reports the **live** one as authorizing (no laundering);
  revoke is idempotent and a forged handle is a no-op; a parent's revocation kills child and grandchild
  transitively (each proven to have worked first, so the test cannot pass vacuously); an interleaved
  revoke swept over **every** position inside a six-step commit body yields a complete effect or a
  denial, never a partial; and revoking one sibling disturbs neither its siblings nor its parent.
- **Two of the new tests failed first, and both were the test's fault** — a hollow assertion
  (`hp >= min(wp, hp)`, trivially true) and a stale waiter model. Both are recorded here because a
  trivially-true assertion is worse than no assertion: it reports coverage it does not have.
- **Register: 15 → 19 resolved, 44 → 40 open.** ALET-P3-003 ("every architectural invariant in one
  place") stays open on purpose — memory, W^X, the namespace and durability carry their contracts in their
  ADRs and in `conformance.sh`, and merging all of it is a bigger job than these four clusters.

Gates after the wave: `build-all` PASS (19 host test binaries green), `e2e-all` PASS, `conformance` PASS
(62 core behaviors × 3 targets), `ci-parity` PASS, `traceability` PASS (68 requirements).

## Previous wave — the OS remembers, and the gate proves it by rebooting (2026-08-03, REQ-STOR-003)

Three waves built storage: an atomic multi-block write, a named namespace, real devices on all three
CPUs. And the **capability-secure spine itself was still rebuilt in RAM at every boot** — not one
entity, not one recorded event survived a power cycle. An operating system that forgets everything at
reset is a demo of an operating system. `kernel-core/src/persist.rs` closes that (ADR-038).

- **An update had to become atomic first.** Saving a store means *updating* an object, and the namespace
  could only create or remove — and "remove then create" is TWO transactions, so a crash between them
  leaves the name **gone**: data loss where the object was merely being updated. `Filesystem::replace`
  commits the new data blocks, the old blocks zeroed (ADR-033), the bitmap and the directory **together**,
  so the name is continuously present and a crash yields the old contents or the new ones. Host-swept at
  **every** crash prefix across five size transitions (same-size, grow, shrink, from empty, to empty).
- **A load verifies; it does not merely parse.** Per entity, the `content_hash` the spine computed is
  recomputed from the bytes actually read — content addressing finally applied to the medium.
- **Writing that test found a real hole, and the fix is in this wave.** A byte-flip sweep over the
  encoded record showed that flipping an entity's **id** produced a store that loaded *successfully with
  different data*: the content hash covers content, and says nothing about id, version, chain,
  provenance or type. The record now carries a trailing checksum over every preceding byte, and the
  sweep asserts the property directly — **any** flip is either refused or yields identical data, and most
  are refused. Content is checked first, so damage to content still reports the precise
  `ContentHashMismatch` rather than the coarser record failure.
- **A corrupt store is a refusal, not a reset.** `open_and_witness` never replaces a store it cannot
  verify with a fresh one: "your data is damaged" must not silently become "your data is gone".
- **Capabilities are deliberately NOT persisted.** What a capability's lifetime means across a reboot is
  ALET-P1-026, still open; writing tokens to disk would be inventing durable privilege by accident.
- **Ids never repeat across a reboot** (`next_id` is part of the record), and the cross-reboot contract
  is one shared function: boot 1 creates the store, boot 2 on the same medium must find and verify boot
  1's entities and report boot number 2.
- **Nine behaviors, every target, in the shared contract.** `ALL 9 DURABLE-STORE INVARIANTS HOLD` on
  aarch64, RISC-V and x86-64 (boot fails `200 + i`); the filesystem suite grew 12 → 15 with the replace
  behaviors, so the real-device suite is now 20; `conformance.sh` requires **61** core behaviors of every
  target, up from 49 — because whether your data is intact must not depend on the CPU.
- **Every write path is now atomic at the same granularity** — block (journal), name (create/remove),
  update (replace), store (save) — and each one is crash-swept at every prefix on the host.
- **Not claimed:** no encryption at rest here (ALET-P1-028/029), no incremental save, no event-log
  persistence, no schema migration beyond refusing an unknown version, and FNV-1a is integrity against
  rot and bugs — not a defence against forgery (that needs REQ-BOOT-002's signing hierarchy).

- **And the gates now prove it by REBOOTING.** Each target's boot gate attaches a **second, persistent
  disk** — the scratch one is reformatted by the destructive suites, this one is created once and kept —
  then boots the same kernel **twice** against it. Boot 1 must report
  `PERSISTENT MEDIUM: boot #1, 0 entities verified`; boot 2 must report
  `boot #2, 1 entities verified`, having loaded and re-verified what boot 1 wrote **through the real
  virtio driver**. On x86-64 the NVRAM copy is per-boot while the disks are not, so the second run is a
  reboot of the same machine rather than a fresh one. That line is now part of the conformance contract
  (**62** core behaviors): "the OS remembers" must not be a property of one CPU.

Gates after the wave: `build-all` PASS, `e2e-all` PASS (aarch64 / riscv64 / x86-64 in QEMU, each booted
TWICE), `conformance` PASS (62 core behaviors × 3 targets), `ci-parity` PASS, `traceability` PASS (68
requirements).

## Previous wave — every target now proves the filesystem on REAL storage (2026-08-03, GAPS4 ALET-P2-019, REQ-DRV-005)

The wave below made the driver shared and gave RISC-V a real disk. x86-64 — the *other* first-class
target — still had none, for a reason no CPU seam could fix: q35 has no virtio-mmio window at all. Its
virtio devices are **PCI functions**, and the registers the protocol needs live inside BAR regions that
a capability list in configuration space points at.

- **The bus became a second seam (ADR-037).** `Transport` names what a bus must provide (feature halves,
  status, queue select/size/addresses/ready, notify, device config). `MmioTransport` serves both QEMU
  `virt` machines; `kernel-x86_64/src/pci.rs` serves virtio-pci. The queue logic, descriptor chains,
  bounded poll and `BlockDevice` impl are untouched: **one protocol, two buses, three CPUs.**
- **One hook, for one real constraint.** virtio-pci's notify address depends on `queue_notify_off`, a
  register *of the selected queue* — reading it inside `notify` would touch `queue_select` with a
  request in flight. `Transport::after_queue_select` (a default no-op the MMIO transport ignores) lets
  the PCI transport latch it once, during setup.
- **The first attempt failed usefully.** QEMU puts the BAR at `0xc000000000`, above 4 GiB, which the
  kernel's own map (ALET-P1-031) deliberately does not cover. Rather than mapping all of physical space
  — trading a precise map for a vague one — **the driver maps its own registers**, and the admission
  check's physical rule is **inverted**: `validate_map` requires the page to be INSIDE the
  frame-allocator window, `validate_map_device` requires it to be OUTSIDE.
- **That inversion is the security content.** A BAR is by definition memory the allocator does not own;
  what must never happen is the reverse — mapping RAM as MMIO would give a task's frame a second mapping
  with different cacheability and side effects, invisible to the ownership model (ADR-030).
  `MapFault::PhysIsRam` names the refusal. The host sweep in `kernel-core/tests/vmaddr.rs` walks the
  whole window plus a margin on all three plans and proves **no page is ever mappable as both**, that
  each rule matches its window exactly, and that the sweep produced both outcomes; x86-64 boot
  invariants 53–55 prove the live API refuses a RAM range while the same page stays a legal RAM mapping.
- **The gate attaches a second disk**, so the boot medium is never written: the scratch disk arrives on
  the virtio-pci bus and the shared 17-invariant suite runs over it. Virtual-memory invariants 52 → 55.
- **Result: all three targets prove the namespace on hardware paths.** aarch64 and RISC-V over
  virtio-mmio, x86-64 over virtio-pci — `ALL 17 VIRTIO-BLK INVARIANTS HOLD` on each. Crash atomicity is
  now a hardware claim on every CPU Aletheia targets, not just the dev backend.
- **Still not claimed:** no DMA isolation (bus-master is now *enabled*, which makes ALET-P1-018 more
  concrete, not less), no interrupts, no multi-queue, no hotplug, no PCI bridge recursion, and no BAR
  assignment — the firmware's placement is used as found.

Gates after the wave: `build-all` PASS, `e2e-all` PASS (aarch64 / riscv64 / x86-64 in QEMU),
`conformance` PASS (49 core behaviors × 3 targets), `ci-parity` PASS, `traceability` PASS (67
requirements).

## Previous wave — a driver belongs to its bus, not to a CPU (2026-08-03, GAPS4 ALET-P2-019, REQ-DRV-004)

The wave below gave every target a filesystem, and the wave before that gave x86-64 its own address
map. Both exposed the same inversion: the only target with a **real block device** was aarch64, which
ADR-019 designates the *bootstrap/dev* backend. So every claim that touched real storage — the
journal's crash consistency, and now the namespace's atomicity — was proved against hardware on the
target that matters least, while the two **first-class** targets proved it against a RAM model.

- **The driver moved to `kernel-core`, not into a second crate (ADR-036).** A split virtqueue, a
  feature handshake and a descriptor chain are facts about **virtio**, not about an instruction set.
  Copying the driver into `kernel-riscv64/` and changing a base address would have been two homes for
  one ring-layout bug — the divergence gap-register Issue 1 exists to prevent, in a path that decides
  what the device is allowed to write into.
- **The seam is two functions.** `VirtioHal::alloc_frame` (a zeroed, *identity-mapped* frame, so the
  address the driver writes through is the address the device DMAs to) and `VirtioHal::barrier`
  (`dsb sy` on aarch64, `fence iorw, iorw` on RISC-V), plus an `MmioLayout` for where a platform puts
  its transports (aarch64: 32 slots 0x200 apart at `0x0a00_0000`; RISC-V: 8 slots 0x1000 apart at
  `0x1000_1000`, inside the device gigapage the identity map already covers). That is the entire list of
  what differs per CPU.
- **`init` returns facts instead of logging them.** An `InitReport` (version, device id, feature halves,
  queue size, capacity) goes back to the caller, which prints it with its own `kprintln!` — a shared
  driver cannot call a per-target macro, and a logging trait for four lines would be worse.
- **RISC-V now proves the filesystem on real storage, first boot.** `device_suite` is shared too:
  discovery → attached geometry → write/read-back round-trip → journal commit + recovery from device
  bytes alone → **the entire 12-behavior namespace over that device** → capability-gated I/O through
  `DeviceGuard`. Seventeen invariants, identical on both targets that have a disk
  (`ALL 17 VIRTIO-BLK INVARIANTS HOLD`; boot fails `120 + i` on aarch64, `180 + i` on RISC-V).
  `scripts/vm-e2e-riscv.sh` now attaches a 1 MiB disk and requires the marker.
- **Geometry is asserted before any byte is trusted.** The suite refuses a device whose block count is
  not the one the gate attached — invariant 2, before any I/O. `kernel-core/tests/virtioblk.rs` proves
  that on the host, and pins the count (17), the dense 1-based numbering and the group order, so a
  suite that quietly stops checking fails `cargo test` instead of passing three QEMU boots.
- **ALET-P2-019 stays deferred, deliberately.** This is progress on the driver *model*, not its closure:
  no hotplug, no interrupt-driven completion (the poll is synchronous, one request in flight), no
  multi-queue, no restart/recovery, and **no DMA isolation** — the device is handed raw physical
  addresses (ALET-P1-018; an IOMMU/SMMU is the real answer). x86-64 still has no block device because
  its transport is virtio-**pci**, which needs PCI enumeration, not an MMIO window — a real bus
  difference, tracked as its own slice rather than papered over.

Gates after the wave: `build-all` PASS, `e2e-all` PASS (aarch64 / riscv64 / x86-64 in QEMU),
`conformance` PASS (49 core behaviors × 3 targets), `ci-parity` PASS, `traceability` PASS (66
requirements).

## Previous wave — the storage stack gets a top: named objects, atomic by construction (2026-08-03, GAPS4 ALET-P2-018)

Until this wave the storage stack was a correct middle with nothing above it. The journal (REQ-STOR-002)
made a multi-block write all-or-nothing; the virtio-blk driver (REQ-DRV-003) made it real hardware; and
every caller above them still addressed **raw block numbers**. Nothing in the system could *name* what
it kept, so every future layer that wanted durable state — an installed component, a policy set, a
content-addressed object — would have invented its own block bookkeeping, and each invention is another
chance to leave the torn state the journal exists to prevent. `kernel-core/src/fs.rs` closes that
(REQ-FS-001, ADR-035), and it is the first of the deferred milestone subsystems to become gated code
rather than architecture text.

- **A name is as atomic as a block.** The directory (one block of 64-byte slots) and the allocation
  bitmap (one block, one bit per data block) are **ordinary home blocks**, so a create commits the data
  blocks *and* the bitmap *and* the directory in ONE journal transaction — and a remove commits the
  zeroed blocks, the cleared bits and the cleared slot together. The classic filesystem crash states
  are therefore not unlikely, they are unrepresentable: no name can point at blocks the bitmap calls
  free, and no allocated block can be owned by no name. **There is no repair pass because there is no
  inconsistent state to repair.**
- **Erase on delete.** A removed object's data blocks are written back as zeros inside the same
  transaction — the storage twin of ADR-033. A block returned to the free map carries none of the bytes
  of the object that used to live there.
- **The bound is a refusal, not a truncation.** A transaction carries at most `MAX_ENTRIES` blocks and
  two slots are always metadata, so an object is bounded at 62 blocks (253 952 B) and a larger one is
  refused with `TooLarge`. Writing a prefix is the single outcome the design exists to exclude.
- **A crash is expressed as a device fault, so the same proof runs on hardware.** `fs::FaultDevice`
  fails every mutation after the first *n* — something the `BlockDevice` trait can already report.
  Hence the atomicity invariants are not a host-only trick: on the host `kernel-core/tests/fs.rs`
  sweeps **every** prefix of a create and of a remove (each outcome must be the whole object or none of
  it, and an unrelated object is asserted to be untouched at every prefix), runs a 4 000-op campaign
  that re-checks the whole-namespace structural invariants after **every single op** against a model,
  and asserts every refusal is a no-op; in-kernel, the same behaviors run at boot.
- **Twelve behaviors, every target, and one target against a real disk.** `fs::selftest_on` runs against
  any `BlockDevice`: all three CPU gates run it over a RAM disk (`ALL 12 FILESYSTEM INVARIANTS HOLD`,
  boot fails `160 + i`), and on aarch64 the identical twelve also run over the **real virtio-blk device
  through the virtqueue** — virtio invariants 5 → 17. `scripts/conformance.sh` now requires all twelve
  of every target: **37 → 49 core behaviors**, because "a create is atomic" must not vary by CPU.
- **Authority is not duplicated.** There is deliberately no capability check inside the namespace; a
  caller wraps the device in `device::DeviceGuard` (REQ-DRV-002) so the same `CapEngine::evaluate` that
  authorizes an entity write authorizes the I/O beneath a name. Two authorization points is a boundary
  that can disagree with itself.
- **Not claimed** (ADR-035 states each): one flat namespace (`/` is refused in names *now* so today's
  names survive a future hierarchy), one bitmap block, contiguous extents only (a create can be refused
  `NoSpace` on fragmentation while the total free count would fit), no rename or in-place update, no
  per-object integrity beyond the commit checksum (post-commit bit rot is still undetected on read —
  the named ADR-024 follow-on), no encryption at rest, no timestamps and no permission bits.

Gates after the wave: `build-all` PASS (5 legs), `e2e-all` PASS (aarch64 / riscv64 / x86-64),
`conformance` PASS (49 core behaviors × 3 targets), `ci-parity` PASS, `traceability` PASS (65
requirements).

## Previous wave — x86-64 stops inheriting the firmware's address space (2026-08-03, GAPS4 ALET-P1-031 / ALET-P2-032)

The wave below took both QEMU targets to zero W^X violations and named what it could not close: on
x86-64 the tree the machine actually translated through was **OVMF's**, holding ~524 795
writable+executable leaves out of ~524 799. That is not a checking failure — long mode requires
paging, so the firmware hands the kernel a running MMU and `ExitBootServices` transfers *ownership of
its hierarchy*, not a chance to build one. No admission check un-maps an inherited page. The kernel
now builds its own map and runs on it, so the last W^X hole in the memory cluster is closed on **all
three** targets.

- **The image describes itself, since the linker does not (REQ-MM-006, ADR-034 addendum).** aarch64
  and RISC-V read `__text_start` / `__text_end` / `__rodata_end` from `linker.ld`; a UEFI PE has no
  such symbols. `kernel-x86_64/src/kmap.rs` takes base + size from `LoadedImage` (captured *before*
  ExitBootServices) and reads the image's own **PE section table** for the rest —
  `IMAGE_SCN_MEM_EXECUTE` and `IMAGE_SCN_MEM_WRITE` carry exactly the facts the linker symbols carry.
  Headers that do not parse make the build **refuse**, rather than defaulting kernel text to writable.
- **Same shape as the other two targets.** Identity, so nothing moves: 2 MiB RW+NX huge pages for RAM
  and sub-4 GiB MMIO, and 4 KiB pages over every 2 MiB region the image touches — text RO+X,
  `.rodata` RO+NX, data/bss RW+NX, headers and padding RO+NX. Every table frame is claimed through the
  ADR-030 ownership model. Measured: 4 GiB covered, 2043 huge + 2560 page leaves, 5 split blocks, 11
  table frames.
- **Built and proved BEFORE activated, because the failure modes differ.** Nine invariants assert not
  "nothing was flagged" but that each class of address is mapped as the right thing: text at 4 KiB
  granularity and RO+X, data RW+NX, `.rodata` neither, bulk RAM a 2 MiB RW+NX block, low memory (the
  SMP trampoline's home) present. Only then does `kmap::activate()` write CR3. A wrong map fails a
  readable invariant instead of triple-faulting the machine with nothing to read.
- **CR4.PGE is cycled across the CR3 write.** A global TLB entry survives a CR3 load *by definition*,
  and OVMF marks its mappings global — so without clearing PGE the firmware's permissions would stay
  live in the TLB for pages this map deliberately narrowed. That is a silent half-switch, which would
  make the W^X claim unprovable rather than false.
- **Everything after the switch runs on the kernel's own tree.** The spine invariants, SMP bring-up
  across four cores, and the entire ring-3 suite — syscalls, per-process address spaces, IPC, grants,
  preemption, priority donation — execute under the map the kernel built. Live audit: **4603 leaves,
  0 violations**, against the inherited tree's 524 795. The boot **fails closed** (exit 28) if the
  live audit ever finds one, and the gate requires the ACTIVE marker + zero live violations instead of
  pinning a bootstrap count. Virtual-memory invariants 40 → **52** on x86-64.
- **The ceiling is an allowlist, not the memory map's maximum.** Firmware describes an aperture
  reaching 1 TiB; taking the raw maximum built a 1 TiB tree (524 283 huge leaves, 1032 table frames =
  4 MiB of page tables) to reach registers this kernel never touches. Only memory types that describe
  real storage raise the ceiling, with a 4 GiB floor for the platform's devices (LAPIC, IOAPIC, HPET,
  framebuffer).
- **The image refusal now lives ONCE (ALET-P2-032).** Splitting an image removes the block/huge
  descriptor that had made its addresses undescendable, so all three targets need the refusal — it was
  written twice, and x86-64 was about to need a third copy, which is exactly what that row predicted.
  `AddrPlan::with_protected` + `MapFault::ProtectedVirt` in `kernel-core/src/vmaddr.rs` hold the rule;
  each target declares only *where* its image is. Host-proved page by page across the span: both APIs
  refuse every page, boundaries are half-open, a malformed protected address still reports the
  malformation first, and an undeclared span protects nothing.
- **And the pair became a shared contract behavior.** The wave below left the two image-refusal
  markers out of `conformance.sh` because x86-64 could not emit them. That reason is gone, so both are
  now core contract behaviors: **37 named behaviors, proved by all three targets** (aarch64 136,
  RISC-V 131, x86-64 129 total invariants). `kernel-core`: 147 host tests pass.
- **Not claimed.** Memory-mapped devices above 4 GiB (64-bit PCI BARs) are unmapped by the x86-64
  tree; nothing this kernel drives lives there, and a driver that needs one must map it explicitly.
  The framebuffer is mapped as Normal write-back memory like the rest of the sub-4 GiB span, because
  x86-64 expresses cacheability through PAT/MTRRs rather than a leaf field.

## Previous wave — W^X becomes global: the kernel image is mapped page by page (2026-08-03, GAPS4 ALET-P1-007)

The previous wave made every *dynamic* mapping prove its permissions, and said plainly what it could
not close: the bootstrap identity map covered the whole kernel image — text, rodata, data, stack, heap
— in single 2 MiB block descriptors, and one descriptor carries one permission set. So 64 descriptors
per QEMU target (all of RAM) were writable **and** kernel-executable, pinned by the gates rather than
hidden. That is the exception that matters most: a writable alias of kernel `.text` means any kernel
write primitive can rewrite the code that is running. It is now closed on both QEMU targets.

- **The linker states the boundaries; the map obeys them (REQ-MM-006, ADR-034 addendum).** Both
  `kernel/linker.ld` and `kernel-riscv64/linker.ld` export `__text_start` / `__text_end` /
  `__rodata_end`. `build_identity` builds every RAM block overlapping `[__text_start, __rodata_end)`
  as a table of 512 4 KiB leaves, each carrying the permissions its **section** deserves: text is
  read-only + kernel-executable, `.rodata` is read-only + execute-never, and everything else (data,
  bss, stack, heap, and RAM merely sharing the block) is writable + execute-never. RAM outside the
  image keeps a single block descriptor — now writable + execute-never at *both* levels. No descriptor
  in the tree is W+X any more, at any granularity.
- **Derived from the image, not restated as a constant.** `image_split_blocks()` computes the affected
  block count from the linker symbols, so a kernel that grows past a block boundary changes the map
  instead of quietly invalidating an assumption. `.rodata` and `.data` both carry `ALIGN(0x1000)`, so
  rounding a section end up to a page can never merge a text page with a rodata page.
- **Both violation classes must now be ZERO, where only one had to be.** Each QEMU gate requires
  `dynamic_violations == 0` **and** `bootstrap_violations == 0` (was `== 64`) from the audit that
  walks the live hierarchy — plus four invariants that prove the split is real rather than assumed:
  the leaf covering `__text_start` is a 4 KiB **page** (not a block) and identity-maps, kernel text is
  executable and read-only and never EL0/U-accessible, `.rodata` is neither writable nor executable,
  and kernel data plus the **running stack** are writable and never executable. Virtual-memory
  invariants 49 → **55** on aarch64 and RISC-V; the audit walks 1087 live leaves on aarch64 and 576 on
  RISC-V with 0/0 violations. Total per-target invariants: aarch64 136, RISC-V 131, x86-64 117 — all
  35 shared conformance behaviors still proved by every target.
- **The split cuts both ways, and that hole is closed in the same wave.** What used to make kernel-image
  VAs unmappable-over was not a rule but a side effect: the level above them was a BLOCK descriptor, and
  `map_page`/`unmap_page` refuse to descend into a block. Replacing those blocks with real tables made
  `map_page(root, __text_start, fresh_frame, NORMAL_PAGE)` **succeed** — a writable page over kernel
  text, precisely the write-to-code path W^X exists to close — and `unmap_page` over kernel `.data`
  reachable too. Neither `validate_map` (a legal 39-bit VA, a pool-owned PA) nor the attribute check (a
  clean RW+NX descriptor) says no. Both APIs now refuse the whole block-aligned split span explicitly,
  and two invariants per target prove it; run against the pre-fix code they fail at invariant 54
  (`exit 114`), so they test the guard rather than restate it.
- **Not added to the shared conformance contract at the time, deliberately.** `conformance.sh` demands
  identically-worded behaviors from all three targets, and x86-64 could not emit this one *then*: its
  image was a PE the firmware loaded and mapped, not a map this kernel built. Requiring it would have
  failed x86-64 for an architectural reason — the same reasoning that omits the ret2usr rule on
  RISC-V's single execute bit. **Superseded by the wave above,** which gave x86-64 its own map and made
  both markers part of the 37-behavior contract.
- **What was left, split out honestly — and since closed.** x86-64 adopted the OVMF tree at
  `ExitBootServices`, and that tree held ~524 795 W^X leaves of ~524 799 — the firmware's map, reported
  informationally at every boot and pinned by the x86-64 gate. ALET-P1-007 was **resolved** here and
  the x86-64 case became its own row, **ALET-P1-031**, whose fix was the backend building its own
  kernel map from the PE image bounds. That row is now **resolved** by the wave above: the firmware's
  tree no longer translates. A note that describes work nobody is doing is the drift ALET-P2-011 exists
  to prevent.

## Previous wave — W^X: permissions are validated, not assumed (2026-08-02, GAPS4 ALET-P1-007 / ALET-P1-008)

The memory model so far answered which memory a mapping may name and who owns it. It said nothing
about what a mapping is allowed to DO — and all three permission mistakes were live in the tree.
aarch64's dynamic kernel page was writable AND kernel-executable; user code pages were writable and
user-executable on aarch64 and RISC-V; x86-64 mapped everything executable because NX was never
enabled (the code said so: "EFER.NXE is not guaranteed by firmware... W^X is not one of the
invariants this milestone proves"). A W+X page is what turns any memory-corruption bug into code
execution.

- **One rule set (REQ-MM-006, ADR-034).** `kernel-core/src/memattr.rs` decodes each target's
  descriptor bits into `PageAttrs { kind, write, exec_user, exec_kernel, user }` and refuses:
  write+execute, executable device memory, a user page executable at kernel privilege (ret2usr), and
  a descriptor claiming user-execute without user-access. Caller-supplied flags are untrusted input
  exactly like `va`/`pa`, so validation happens where they enter.
- **Real mappings changed, not just checks added.** User code pages are now **read-only +
  executable** on all three targets (the stub is written through the frame's kernel identity address
  before the user mapping exists). aarch64's dynamic kernel page gained `PXN`. x86-64 marks writable
  pages `NO_EXECUTE`, which meant actually **enabling `EFER.NXE` and `CR4.SMEP`** after a CPUID
  check, and printing what the CPU allows instead of assuming it.
- **A checker, because API enforcement is not proof about the tree.** `memattr::audit` walks a live
  hierarchy through the same `TableOps` seam reclamation uses and counts violations by class. Every
  gate requires **zero** among the mappings that kernel created (virtual-memory invariants 42 → **49**
  on aarch64 and RISC-V, 33 → **40** on x86-64; 35 shared conformance behaviors, up from 31).
- **Per-architecture honesty rather than a lowest common denominator.** aarch64 has separate
  UXN/PXN, so all three rules are expressible. RISC-V Sv39 has ONE execute bit qualified by `PTE_U`,
  so "user-accessible AND kernel-executable" is *unrepresentable* — its gate proves the U-mode W+X
  analogue and the shared contract omits the rule instead of pretending. x86-64 also has one NX bit,
  so a USER page with NX clear is ring-0-fetchable by paging alone; `CR4.SMEP` is what forbids it,
  and the decode says so.
- **Delivered as `partial`, deliberately.** ALET-P1-008 (attribute validation) is **resolved**.
  ALET-P1-007 (W^X as a COMPLETE global invariant) stayed **open** at the time of this wave: aarch64
  and RISC-V identity-mapped the kernel image in 2 MiB blocks spanning text, rodata, data, stack and
  heap together, so **64 W^X block descriptors remained on each — a number the gates PINNED**, so the
  exception could not grow unnoticed. On x86-64 the inherited OVMF tree holds ~524 795 W^X violations
  across ~524 799 leaves; it is the firmware's map, reported informationally at every boot. Closing
  this needed a page-granular kernel-image split via linker symbols — **done by the 2026-08-03 wave
  above**, which took both QEMU targets to zero bootstrap violations and left only the x86-64
  firmware tree (ALET-P1-031) — **and that too is now closed**, by the latest wave: x86-64 builds its
  own map from its PE image bounds and CR3 points at it, so W^X is a live global invariant on all
  three targets.

## Previous wave — Memory safety: a freed frame carries nothing (2026-08-02, GAPS4 ALET-P2-026)

The previous three waves made frame ownership explicit, so two owners can never hold one frame at
the same TIME. That is a temporal guarantee, and it was silent about what the NEXT owner could READ.
A frame returned to the pool kept its bytes verbatim — and the pool is LIFO, so the very next
`alloc`, in any address space, for any task, was usually that exact frame. Keys, plaintext message
bodies, decrypted store content and IPC payloads all travelled that way. Every return path fed it:
explicit frees, page-table reclamation, and address-space destruction — which is precisely the path
a *crashing* task takes. The previous three wave entries each disclosed this as still-open; this
closes it.

- **Erase at release, not at allocation (REQ-MM-005, ADR-033).** Each target's `free_as` zeroes the
  whole frame once the ownership check has confirmed the caller really held it, and before the
  free-list link word is written. A refused free still erases nothing — it remains a total no-op.
  Because the erase sits in the one choke point every return path shares, reclamation and teardown
  inherit it without knowing it exists, and no caller can opt out by using plain `alloc`.
- **The guarantee is stated precisely: no frame ever carries a previous OWNER's bytes.** It is NOT
  "every allocation returns zeros" — a frame that has never been owned still holds whatever firmware
  left there. That is pre-boot memory, not another task's data. `alloc_zeroed` is kept deliberately
  for callers that need a guaranteed-blank page (page tables demand it), not by oversight.
- **Proved by the reuse case, which is the only honest proof.** Each gate writes a recognizable
  pattern across a frame, frees it, allocates again, asserts it got the SAME frame back (LIFO), and
  requires every word past the free-list link to be zero. Asserting that `alloc_zeroed` returns zeros
  would prove nothing about what a plain `alloc` hands the next task. Memory invariants 17 → **21**
  on aarch64 and RISC-V, → **22** on x86-64.
- **Part of the contract.** `scripts/conformance.sh` requires the erase behavior from all three
  targets (31 core behaviors, up from 29): a CPU on which a reused frame still holds the last owner's
  bytes is a cross-task information leak, whatever its instruction set.
- **Honesty about the x86-64 clamp.** `init_from_uefi` clamps the managed window to what the
  ownership state array covers, but under the gate's `-m 256` the pool is far below that ceiling, so
  **the clamp branch itself never executes in CI**. What the gate now proves is the invariant the
  clamp exists to maintain — every managed frame has ownership state (`total_count() <= MAX_FRAMES`).
  The clamp path remains unexercised, and is recorded as such here rather than counted as covered.

## Earlier wave — Memory safety: a dying address space gives everything back (2026-08-02, GAPS4 ALET-P1-004)

Fourth and final slice of the P1 memory-safety cluster's reclamation arc, and the one the previous
three unlocked. Reclamation (ALET-P1-002) serves a task that tidies up page by page. A task that
simply DIES — faults, is killed, or exits without unmapping — used to keep everything it held
forever: its user pages, every intermediate table, and its root. An OS where process death leaks
memory has no process lifetime worth the name; a crash loop is a slow, unattributable exhaustion.

The hard part is not walking the tree, it is that **a page-table tree is not a private forest**:
x86-64 builds a per-process PML4 by COPYING the live one, so almost every top-level slot points at
firmware and kernel tables the running kernel still needs; aarch64 and RISC-V per-process roots carry
an identity map of 2 MiB block / megapage descriptors that were never pool frames; and a page shared
through the grant table is mapped here but owned elsewhere. A naive recursive free takes the machine
down on its first teardown.

- **One arch-independent walk, two independent guards (REQ-MM-004, ADR-032).**
  `kernel-core/src/teardown.rs` owns the traversal; each target implements `SpaceOps` on top of the
  `TableOps` it already wrote for reclamation. **Privacy**: `is_private(level, index)` says which
  slots are this space's own — x86-64 scopes the walk to PML4 slot 0's privatized PDPT and its one
  1 GiB user region, while the QEMU `virt` targets declare every slot private. **Ownership**: every
  free goes through the model from ADR-030, so a block descriptor, a device mapping or a granted
  page is refused and reported as SKIPPED. Neither guard covers for the other by accident: if the
  privacy predicate were wrong, ownership still refuses.
- **Order is the safety property.** Depth-first, children before parents, root last, and every entry
  zeroed before its target is freed — no freed frame is ever still reachable. There is deliberately
  no restore-on-refusal (unlike reclamation): the space is dying, so a cleared entry to a frame we
  could not free is the correct end state. Refusals are counted (`tables_refused`), not swallowed.
- **Destroying the space you are running in is refused** on all three targets — the kernel cannot
  free the ground beneath itself — and that refusal is part of the conformance contract.
- **VM-gated on live hierarchies, count-pinned.** Virtual-memory invariants go 33 → **42** on
  aarch64 and RISC-V and 25 → **33** on x86-64: a second address space is built and populated, the
  active-root teardown is refused, exactly the owned pages come back, unowned block descriptors are
  skipped, no table is refused, **the free count returns EXACTLY to its pre-space value**, the freed
  pages have no owner, and on x86-64 the copied kernel slot the victim shared is byte-identical
  afterwards.
- **What this completes.** With address admission (ADR-029), ownership (ADR-030), reclamation
  (ADR-031) and destruction (ADR-032), physical memory is conserved across the whole lifetime of an
  address space: frames cannot be aliased, double-freed, leaked by unmapping, or leaked by dying.
- **Scope, stated honestly.** Freed frames are NOT zeroed before reuse (ALET-P2-026 — a page's bytes
  are still readable by its next owner), W^X is still not a global invariant (ALET-P1-007), and
  per-arch memory-attribute validation (ALET-P1-008) is untouched. All **open** in the register.

## Earlier wave — Memory safety: an unmap gives the page tables back (2026-08-02, GAPS4 ALET-P1-002)

Third slice of the P1 memory-safety cluster, landed the same day as the frame-ownership model it
depends on. Mapping one page allocates a chain of translation tables (L2+L3 on the 3-level aarch64
TTBR0 and RISC-V Sv39 walks; PDPT+PD+PT on x86-64's 4-level walk). Unmapping it cleared the leaf
entry and stopped — every intermediate table stayed **allocated and still referenced**.

At boot-test scale that was a bounded, documented leak. As a running-OS property it is a denial of
service: a task that maps and unmaps across a wide virtual range consumes one frame per 512-page
span it has ever *visited*, regardless of how few pages it *holds*, and any unprivileged task can
drive that with a loop. It also blocked address-space teardown outright, since nothing knew which
tables belonged to a space.

- **One arch-independent rule set (REQ-MM-003, ADR-031).** `kernel-core/src/ptreclaim.rs`: a table
  is freed only when EVERY entry is absent; the parent reference is cleared BEFORE the frame is
  freed (free-then-clear leaves a live entry pointing at a re-allocatable frame); the root is never
  freed; the walk stops at the first table still in use; and a refused free RESTORES the parent
  entry, because a refusal must not leave a table unreachable-but-allocated. Each target implements
  a four-method `TableOps` over its own descriptor format — the only architectural knowledge is the
  present bit and the entry width.
- **Ownership-checked, which is why it is safe.** Tables are freed as `Owner::PAGETABLE` through the
  allocator from ADR-030, so reclaiming a user page, another space's frame, or an already-free frame
  is refused rather than obeyed. This wave was only buildable because that one landed first.
- **Invalidation stays architectural.** Detaching an ancestor can leave stale paging-structure
  (walk) cache entries. Every target calls reclamation BEFORE the invalidation it already did for
  the unmapped VA, so one `tlbi vae1` / `sfence.vma` / `invlpg` covers the leaf and its detached
  ancestors. On x86-64 the walk path is captured before `Mapper::unmap` runs, while the chain is
  still intact.
- **VM-gated on live hierarchies, count-pinned.** Virtual-memory invariants go 21 → **33** on
  aarch64 and RISC-V and 13 → **25** on x86-64: mapping consumes table frames; unmapping one of two
  pages in a leaf table reclaims NOTHING and the sibling still resolves; emptying the table returns
  every intermediate frame to the allocator; neither VA resolves afterwards; and the address space
  REBUILDS the chain, which proves the root survived and the freed frames were genuinely reusable.
- **Contract by behavior, not by count.** A 3-level walk reclaims two tables where a 4-level walk
  reclaims three — an honest architectural difference. `scripts/conformance.sh` requires the five
  reclamation *behaviors* from all three targets (24 core behaviors, up from 19) and lets the counts
  differ.
- **Scope, stated honestly.** This reclaims tables an unmap empties. It does NOT free the tree of a
  dying address space (ALET-P1-004, now unblocked), zero reclaimed frames (ALET-P2-026), or enforce
  W^X (ALET-P1-007) — all still **open** in the GAPS4 register.

## Earlier wave — Memory safety: a frame has an owner (2026-08-02, GAPS4 ALET-P1-003)

Second slice of the audit's P1 memory-safety cluster, and the one the reclamation work was waiting
on. Every target runs the same intrusive free-list allocator, whose `free` checked only two things:
is the address 4 KiB-aligned, and is it inside the managed window. Both are true of a frame that is
**already on the free list** — so a double free pushed one frame onto the list twice, and two later
`alloc` calls handed ONE physical page to two owners (typically a page table and a user page over
one another). Both are equally true of a frame that is **live in another address space**, so a
caller could donate someone else's page to the next allocation. Neither faults; the address space
quietly aliases and the corruption surfaces much later as a wrong-page read.

- **One arch-independent model (REQ-MM-002, ADR-030).** `kernel-core/src/frameown.rs`:
  `FrameOwnerTable` keeps one byte of state per frame — free, an owner tag, or permanently reserved
  — and every allocator transition is checked against it. Ownership is *claimed before the frame
  leaves the list* and *released only by its holder*, so `AlreadyOwned`, `NotOwned` (the double
  free), `WrongOwner` and `Reserved` are named, fail-closed refusals rather than a bare `false`.
- **The tags describe the kernel's real structure.** Page tables are `Owner::PAGETABLE` and
  EL0/U-mode/ring-3 pages are `Owner::USER` on all three targets, with `Owner::address_space(id)`
  reserved for per-address-space identities. `transfer` moves ownership atomically, because doing it
  as release+claim would leave the frame momentarily allocatable by a third party — the same
  reasoning as `CapEngine`'s atomic authorize-and-execute (REQ-CAP-006).
- **Proved as properties, not examples.** `kernel-core/tests/frameown.rs` drives a deterministic
  20 000-operation mix of legal and illegal calls and asserts after EVERY step that no frame is held
  by two owners, that `owned + reserved + free` still equals the window, and that every refusal left
  the whole table byte-identical. (The first run of that suite caught its own weakness: an LCG's low
  bits cycle with period 4, so `next() % 4` produced 1 accepted operation in 20 000 — the generator
  now returns high bits, and the suite asserts both paths were exercised.)
- **VM-gated on live pools, count-pinned.** Each target's memory suite goes 7 → **17** invariants,
  run against the real global allocator: an allocated frame reports its owner, a cross-owner free is
  refused and moves nothing, a double free is refused without pushing the frame twice, a
  never-allocated frame cannot be freed, a legal free still succeeds afterwards, and the ownership
  table's free count matches the allocator's own. All three gates now pin the count, so losing an
  invariant fails instead of passing quietly.
- **Part of the cross-architecture contract.** `scripts/conformance.sh` requires the five ownership
  refusals, identically worded, from all three targets (19 core behaviors, up from 14): a CPU on
  which a double free is accepted is a different memory-safety boundary, not a detail.
- **A qualification hole found on the way.** `kernel-x86_64/scripts/build-image.sh` (and its Linux
  twin) skipped `cargo build` whenever an `.efi` already existed, so an edited tree produced an image
  built from the PREVIOUS binary — the boot gate passed against code no longer in the tree, which is
  how the first x86-64 run of this wave "proved" 7 invariants. Both scripts now always compile;
  `EFI=/path` remains the deliberate escape hatch.
- **Scope, stated honestly.** This closes frame *ownership*. Page-table reclamation (ALET-P1-002),
  address-space destruction (ALET-P1-004), zeroing on free (ALET-P2-026) and a global W^X invariant
  (ALET-P1-007) remain **open** in the GAPS4 register — but 002 and 004 are now unblocked, since both
  free frames in bulk and needed an owner to check against.

## Earlier wave — Memory safety: the mapping API stops trusting raw addresses (2026-07-28, GAPS4 ALET-P1-001)

First slice of the audit's P1 memory-safety cluster. Every target's dynamic mapping API took a raw
`va`/`pa` and walked straight into live page tables. A walker decodes a fixed VA width (39 bits on
aarch64 TTBR0, 39 on RISC-V Sv39, 48 on x86-64), so bits above it are not part of the walk — two
different virtual addresses **alias the same page-table entry**: a second map silently overwrites the
first, and unmapping one tears down the other. Same class on the physical side: a misaligned `pa` has
its low bits swallowed by the entry's address mask, and a `pa` outside the frame allocator's window
maps firmware tables, MMIO, or another address space's frames. On x86-64 two of these were not even
logical faults — `Page::containing_address` silently truncates a misaligned VA to its page base, and
`VirtAddr::new` *panics* on a non-canonical address, turning caller-supplied input into a kernel abort.

- **One arch-independent rule set (REQ-MM-001, ADR-029).** `kernel-core/src/vmaddr.rs`: each target
  declares an `AddrPlan` once — decoded VA width, whether the ISA requires canonical sign-extension,
  and the physical window, read from the frame allocator at call time so the check cannot drift from
  the pool it protects. Refusals are typed (`MapFault`), fail-closed, at the entry of every mapping
  API on all three targets.
- **`canonical` is a real architectural difference, not a style flag.** aarch64 TTBR0 (T0SZ=25)
  covers a flat `[0, 2^39)` with TTBR1 disabled, so every higher bit must be zero. x86-64
  sign-extends from bit 47. **RISC-V Sv39 sign-extends from bit 38** — its 39 bits *include* the sign
  bit, so its low half is `[0, 2^38)`; modelling it like aarch64 would have wrongly accepted
  `[2^38, 2^39)`. Each plan is judged by its own ISA rule.
- **Proved as properties, not examples.** `kernel-core/tests/vmaddr.rs` enumerates candidate
  addresses across every target plan and asserts the two properties the rules exist to guarantee: no
  two *accepted* virtual addresses may alias, and the exact alias of every accepted address is
  refused; every *accepted* physical address is a frame the allocator owns.
- **VM-gated on live page tables.** aarch64 21 and RISC-V 21 virtual-memory invariants (was 13 each),
  x86-64 13 (was 6). Each target holds a still-allocated frame across the block, so every refusal is
  attributable to the address rather than to allocator exhaustion, and each ends by proving a legal
  map/translate/unmap still succeeds — "everything is refused" cannot masquerade as a pass. The
  gates now pin the invariant COUNT, so losing an invariant fails instead of passing quietly.
- **The refusals are part of the cross-architecture contract.** `scripts/conformance.sh` requires the
  four identically-worded refusals from all three targets: a target that accepts an address the
  others refuse is a security boundary that varies by CPU.
- **Scope, stated honestly.** This closes address *admission* only. Frame ownership / double-free
  defense (ALET-P1-003), intermediate page-table reclamation (ALET-P1-002), address-space destruction
  (ALET-P1-004) and a global W^X invariant (ALET-P1-007) remain **open** in the GAPS4 register.

## Earlier wave — Qualification infrastructure (2026-07-24, GAPS4 P0 cluster)

Closing the audit's #1 risk (`docs/gap/ARCHITECTURE-GAPS4.md`): "qualification systems are behind
the number of architectural claims." Disposition of all 67 findings lives in
`docs/gap/ARCHITECTURE-GAPS4-REGISTER.md`.

- **ALET-P0-001 — x86-64 boot gate at CI parity (RESOLVED).** x86-64 was first-class in the
  architecture but had no automated boot gate (its ring-3/syscall/timer/paging code could regress
  while CI stayed green). Root cause the gate didn't exist: `build-image.sh` is macOS-only
  (hdiutil/diskutil). Added `kernel-x86_64/scripts/build-image-linux.sh` (portable FAT ESP via
  `mtools`, no root/loop devices), generalized `smoke-test.sh` OVMF discovery to Linux, added
  `scripts/vm-e2e-x86.sh`, and wired the `vm-e2e-x86` CI job — booted green under QEMU+OVMF at
  `-smp 4` (exit 33, 22 ring-3 + memory + vm + spine + SMP markers).
- **ALET-P0-002 — single repository-wide integration build (RESOLVED).** `scripts/build-all.sh`
  builds every crate with its own pinned toolchain/target (host crates also tested) with one
  aggregate pass/fail; wired as the `build-all` CI job. `E2E-ALL: PASS` across all three CPU targets.
- **ALET-P0-003 — CI-coverage gate: claims must RUN, not merely exist (RESOLVED).**
  `check-traceability.sh` proves a `delivered` requirement points at evidence that exists; it cannot
  prove CI executes that evidence. ALET-P0-001 was exactly that hole — an x86-64 boot script on disk,
  named in the matrix, run by nobody. `scripts/check-ci-parity.sh` (REQ-QUAL-001) closes it with four
  mechanical checks: (1) every **bootable kernel crate discovered from the tree** (`Cargo.toml` +
  `src/main.rs`) must have a boot gate CI executes — so adding a fourth CPU target without a gate
  fails the build instead of shipping an unqualified architecture; (2) `.github/workflows/ci.yml` and
  `.gitlab-ci.yml` must execute the **same** script set, because the repo pushes to both GitHub and
  the self-hosted GitLab origin and a one-sided gate is enforced for half the pushes; (3) every path
  in the matrix's `VM Gate` column must actually be executed by CI — no "aggregate runner" exemption,
  since a wrapper can carry assertions of its own that nothing would run; (4) STATUS.md must name
  every script CI runs. Comments are stripped before resolution, so a script mentioned in prose can
  never stand in for a job.
- **Wiring the gate found real gaps, now fixed.** `.gitlab-ci.yml` was missing `build-all` and
  `vm-e2e-x86` (they existed only on GitHub); `scripts/conformance.sh` (REQ-CONF-001, the
  cross-architecture semantic-divergence gate) was claimed as a VM gate but had **no CI job at all**,
  and its x86-64 column was hard-gated on macOS/`hdiutil` so it could never compare x86 in CI. Both
  pipelines now run `build-all`, `vm-e2e-x86`, `conformance`, `traceability` and `ci-parity`, and
  `conformance.sh` drives the portable `scripts/vm-e2e-x86.sh` leg. The matrix's x86-64 gate is now
  canonically `scripts/vm-e2e-x86.sh` (the CI-executed leg; `smoke-test.sh` is the boot step it
  invokes), and REQ-SMP-001 names the three legs it relies on rather than the `e2e-all.sh` wrapper.
- **Reliability fix (root cause of local `E0463 can't find crate for core` / `FAIL: build`):** a
  Homebrew/system `cargo` earlier in `PATH` ignores each kernel's `rust-toolchain.toml` and builds
  for the host triple. All build/boot scripts now prepend the rustup shim (`~/.cargo/bin`).

## Delivered (M1)

A Rust hosted reference implementation of the System Core (`aletheia/`), enforcing the same invariants
a microkernel will later enforce (ADR-010, contract-honest). Runs in userspace; no hardware needed.

- Semantic store: content-addressed, versioned, **encrypted at rest** (ChaCha20-Poly1305), durable.
- Capability engine: possession-based **unforgeable** tokens, attenuated delegation, cascading
  revocation, **fail-closed** ALLOW / DENY / REQUIRE_APPROVAL.
- Intent→Action pipeline: interpret (only probabilistic stage) → parse → validate → authorize →
  approve → execute → **verify** → record immutable event + full trace.
- Intelligence runtime: `ModelRuntime` port + deterministic interpreter fallback + local-model adapter.
  OS is fully functional with no resident model (INT-004).
- Agents: first-class, capability-bounded, revocable actors.
- World model, thin context/memory, tools registry, hosted experience surface (`aletheiad` renders
  explainable traces + world model + audit log).

### 20 M1 acceptance criteria → tests (all green: `cargo test` = 18 passed)

| # | Criterion | Test |
|---|-----------|------|
| 1 | Entity created, content-addressed, retrievable | `spine::spine_end_to_end` |
| 2 | Versioning; prior recoverable | `acceptance::c2_versioning_and_recovery` |
| 3 | Encrypted + survives restart (plaintext absent from disk) | `spine` (raw-bytes + restart) |
| 4 | Relationships + world-model traversal | `acceptance::c4_relationships_world_model` |
| 5 | Every action needs a capability (fail closed) | `spine` + `acceptance` |
| 6 | Capabilities unforgeable (forgery denied) | `acceptance::c6_capabilities_unforgeable` |
| 7 | Delegation attenuation (no amplification) | `acceptance::c7_delegation_attenuation` |
| 8 | Revocation propagates | `acceptance::c8_revocation_propagates` |
| 9 | Destructive requires approval | `acceptance::c9_destructive_requires_approval` |
| 10 | Intent interpreted then validated before execution | `spine` |
| 11 | Malformed model output cannot execute | `acceptance::c11_malformed_output_cannot_execute` |
| 12 | Mid-flight interpretation failure is safe | `acceptance::c12_midflight_interpretation_failure_is_safe` |
| 13 | Verified against real store before success | `spine` |
| 14 | Immutable event with full trace | `spine` |
| 15 | Agent bounded by its capabilities; revocable | `acceptance::c15_agent_bounded_by_capabilities` |
| 16 | Cancellation stops without side effects | `acceptance::c16_cancellation_stops_without_side_effects` |
| 17 | Operates with no resident model | `acceptance::c17_operates_without_model` |
| 18 | No ambient authority | `acceptance::c18_no_ambient_authority` |
| 19 | Untrusted content is data, not instruction | `acceptance::c19_untrusted_content_is_data_not_instruction` |
| 20 | Experience surface renders full trace | `acceptance::c20_experience_surface_renders_trace` |

Plus `security.rs`: expired-capability denial, scope confinement, agent-cannot-self-escalate.

## Deferred (documented, not coded — by design; see PRD §41 / SAD §22)

- **P2** (partially delivered — see "Delivered (P2 start)" below) WASM/WASI capability-secure
  component runtime + SDK + multi-agent composition. The runtime + app-as-capability model + fuel
  bounding + a content return-buffer (read→transform→write) + multi-agent composition (spawn) + the
  **Rust component SDK** + the **property/chaos gate** are delivered and tested; only the longer-running
  soak/adversarial stress campaigns remain.
- **P3** Native-architecture experience layer (workspaces, dynamic interfaces, semantic search).
  *(Started: capability-gated keyword search over the World Model is delivered — see the P3 section.)*
- **P4** Real microkernel (Rust) on metal: capability enforcement, secure IPC, memory/address spaces,
  interrupts; System Core rehosted on it. VM-tested.
- **P5** (partially delivered — see "Delivered (P5 start)" below) real memory management: a physical
  page-frame allocator + MMU virtual memory (identity map + dynamic map/unmap) are delivered and
  VM-tested on the aarch64 dev backend. Still deferred: higher-half (TTBR1) split, timer-driven
  preemption (GIC), HAL on real devices, native on-GPU compositor, heterogeneous CPU/GPU/NPU scheduler,
  secure boot, rollback/recovery.
- **P6** Optional sandboxed Linux/POSIX compatibility environment (see Compatibility Appendix).

These require hardware/GPU/kernel and are not testable in a hosted dev environment; they get
architecture text and phased plans, not blind code (ADR-010).

## Engineering notes

- **Rust-first** (ADR-004): 100% safe Rust; no C toolchain in M1 (`sha2`/`chacha20poly1305` are
  pure-Rust). C/C++/asm only behind audited FFI in later hardware phases.
- **Single crate, module boundaries** mirror the SAD's crate list; splitting into a cargo workspace is
  a mechanical later step (dependency direction already points inward toward `domain`).

## Delivered (P2 start — WASM capability-secure components)

A `wasmi`-based component runtime (`aletheia/src/component.rs`, ADR-014) that runs **untrusted**
WASM as first-class applications while preserving every M1 invariant. This is the layer that lets
Aletheia actually run programs — and it does so with **no ambient authority**.

- **No WASI.** A component reaches the OS only through an explicit host ABI (`read` / `write` /
  `emit` / `spawn`). There is deliberately no ambient filesystem/clock/rand/env. `read` copies
  authorized content into a guest-supplied return buffer, so a component can consume and compute over
  the data it is allowed to read (proven by an end-to-end read→transform→write program).
- **Multi-agent composition.** A component can `spawn` an installed child component; the System Core
  runs the child with a capability **attenuated** (delegated) from the parent — so a child can never
  exceed its parent's authority (a read-only parent cannot hand a child write). Spawn depth is bounded.
- **Same authority mechanism.** Every host call authorizes through the *same* `CapEngine::evaluate`
  the deterministic pipeline uses, against the exact capabilities the component was granted —
  nothing inherited from the launcher.
- **Application-as-capability.** Launching a component at all requires a `component.run` capability;
  the component then executes with *exactly* its `grant_caps`. `install_component` registers WASM as
  an encrypted, content-addressed `Application` entity; `run_installed` launches it from the store.
- **Same audit log.** Allowed effects (entity writes, event emits) land in the one immutable event
  log with the component as actor; every host-call attempt (allowed or denied) is in an explainable
  per-run audit.
- **Fuel-bounded.** A runaway component is trapped out-of-fuel and leaves no effects — it cannot
  hang the OS (pre-stages the P2 stress/chaos gate).

**14 P2 acceptance tests** (`tests/component.rs`, all green; 2 are property/fuzz) prove the core
invariant: a component with no capability can do nothing; with an attenuated grant it can do exactly
that and no more; every effect is traced; reads and writes are capability-gated; a runaway is bounded;
launching is gated; an installed component runs from the store; an approval-required capability is
refused at the component boundary (criterion 9 preserved); a component reads→transforms→writes real
data end to end; a committed effect survives a later fuel-kill (a trap cannot corrupt state); a
component spawns a child that runs under a delegated capability; and a spawned child cannot exceed its
parent's authority. The untrusted host-ABI boundary is **fuzzed** (PRD §38.4): the fail-closed default
and host robustness hold for randomized memory arguments no one enumerated. The **component SDK** is now
delivered (Rust authoring layer over the host ABI — see its section below), and the gating
property/chaos campaign is now green (`tests/component_chaos.rs`, below). Deferred (follow-on P2
iterations): richer parent→child data-flow wiring, and longer-running soak/adversarial stress.

## Delivered (2026-07-21 — P2 component SDK: author components in Rust)

The layer that lets a developer **write an Aletheia component in Rust** instead of hand-assembling
WASM/WAT against the raw host ABI. A small `no_std` crate (`component-sdk/`, `aletheia-component-sdk`)
wraps the four capability-gated host calls behind safe, typed functions — and nothing else, because
there is nothing else to reach (no WASI, no ambient authority; ADR-014).

- **API**: `write_output(&[u8])`, `emit_event(&str)`, `read_entity(&str, &mut [u8]) -> len`,
  `spawn_child(app, action)` — each returns `Result<_, HostError>` where `HostError` maps the host's
  ABI sentinels (`-1` Denied / `-2` NeedsApproval / `-3` Bad). A `component_main!` macro exports the
  guest's `run() -> i32` and provides the `no_std` `#[panic_handler]` (a panicking component traps and
  leaves no effects — the host's per-call all-or-nothing + fuel boundary already guarantees this).
- **Example**: `examples/hello-component/` — a real guest authored with the SDK (writes an Output
  entity, emits an event), compiled to `wasm32-unknown-unknown`.
- **Verified by the SAME bar as the runtime**: `aletheia/tests/sdk_component.rs` (4 tests, green) runs
  the SDK-authored guest through the *unchanged* `SysCore` and asserts it is exactly capability-bounded
  — no capability ⇒ it changes nothing (write denied, exits 1, zero effects); granted `entity.write` +
  `event.emit` ⇒ it does exactly those, exits 0, and the stored entity holds precisely the bytes the
  SDK wrote; granted only `entity.write` ⇒ it writes but its emit is denied (attenuation, exits 2).
- **No new CI toolchain dependency**: the example is prebuilt to a committed fixture
  (`aletheia/tests/fixtures/hello_component.wasm`, 306 B) by `scripts/build-example-component.sh`, so
  the hosted `cargo test` gate stays green with no wasm target required; regenerate the fixture with
  that script (needs `rustup target add wasm32-unknown-unknown`) whenever the SDK or example changes.
  `clippy -D warnings` clean on host + wasm32.
- **Deferred (follow-on)**: an `alloc`-backed convenience layer (owned buffers for `read_entity`),
  richer parent→child data-flow, and typed entity/metadata helpers.

## Delivered (2026-07-21 — P3 start: capability-gated World-Model search)

The first slice of the P3 experience layer: **search over the World Model**, subject to the same
capability discipline as everything else. `ContextEngine::search_world` (ADR-018's search seam, in
its always-available NO-embedding form — the `SemanticRetriever` embedding path stays an optional
extension) scores entities by keyword match across type/metadata/UTF-8 content and returns the top
hits most-relevant-first (deterministic; ties broken by id). It is **authorization-before-inclusion**:
only entities the caller may `entity.read` are ever considered, so an unauthorized entity never
appears even when it matches, and a caller with no read authority gets nothing (fail closed). Exposed
as `SysCore::search(offered, query, limit)` (read-only). Verified by `aletheia/tests/search.rs`:
capability-gated (a reader scoped to one entity never sees another that matches), ranked (a two-term
match outranks a one-term match), and fail-closed. Deferred (P3): embedding-backed semantic search,
workspaces, and dynamic interfaces.

## Delivered (2026-07-21 — P2 property/chaos gate)

The runtime's two load-bearing invariants, proved over the RANDOMIZED space the fixed tests don't
enumerate — `aletheia/tests/component_chaos.rs` (3 tests, green; 2 are `proptest` campaigns of 64
cases each): for any random (capability-set × host-call-sequence × fuel), (1) **no effect without a
capability** and (2) **effects ⊆ grant** — and fuel exhaustion can only *reduce* effects, never
manufacture an unauthorized one, and never hangs the OS (every run returns a verdict). Plus a
cross-run isolation test: authority never leaks from a privileged component run to a later
unprivileged one. Hosted suite now **66 passed**. Deferred: longer-running soak / adversarial stress.

## Delivered (P4 start — VM-tested microkernel)

A `no_std` Rust microkernel (`kernel/`) that **boots on QEMU `virt` at EL1** and re-proves the
M1 invariants **in kernel space** — the first executed instance of the PRD's VM-Testing layer
(ADR-012, ADR-013), done contract-honest (ADR-010: no blind hardware code; this runs).

- Boot: stack/BSS/heap (bump alloc), PL011 UART, EL1 exception vectors, ARM semihosting exit.
- In-kernel capability-secure spine: content-addressed store, capability engine with an
  **unforgeable-by-construction** `CapToken` (private id field — stronger than the hosted
  string token), and the validate→authorize→execute→verify→record pipeline + secure IPC.
- **11 in-kernel invariant selftests** (M1 acceptance, re-proved live) drive the VM exit code;
  all green. `scripts/vm-e2e.sh` is the CI VM gate (build→boot→assert→exit 0).
- **Performance validation** (QEMU TCG; same emulated CPU, same run — substrate-fair ratio):
  the capability authorization check Aletheia *adds* ≈ **0.79× one bare `svc` trap** (two
  checks) — cheap. This does **NOT** show Aletheia IPC < Linux IPC: the measured loop runs in
  EL1 and crosses no privilege/address-space boundary, while a real microkernel IPC AND a Linux
  pipe both pay ≥2 crossings + context/AS switches. Whole-OS "faster than Linux" stays a
  benchmark program (cross-AS IPC vs a same-emulator Linux guest = next milestone), never a
  claim; and **not** bare-metal numbers.

## Delivered (Alpha wave — 2026-07-21): policy engine, AI subsystem, Context Engine

Elevating the M1 reference from a scripted demo toward the real layered architecture the SAD (§3/§4,
§10) already commits to — the code now catches up to the docs.

- **Policy & approval engine** (`policy.rs`, ADR-015) — a governance axis SEPARATE from capability
  authority. Capabilities decide *authorized?*; policy decides *needs human approval?*. Both approval
  triggers (destructive risk + approval-constrained capability) unified in one place. Durable
  pending-approval lifecycle (request → list → grant/deny → execute) persisted via the immutable
  event log and replayed on open; **approval confers no authority** (caps re-evaluated on execution).
- **AI subsystem** (`ai/`, ADR-017) — AI as a first-class Aletheia-owned subsystem behind a
  model-agnostic `ModelProvider` (`config`/`provider`/`context`/`prompt`/`runtime`/`llama`). Primary
  hosted model = `GnLOLot/MiniCPM5-1B-Claude-Opus-Fable5-V2-Thinking-GGUF` (Q8_0) resolved from the
  HF cache by configurable reference (weights never in git); deterministic interpreter is fallback +
  test oracle. **Live-validated** against the running model: no-think mode + GBNF grammar yields
  correct plan JSON; a strict grammar alone collides with the model's `<think>` phase.
- **Context Engine** (`ai/context.rs`, ADR-018) — native capability-aware **Context Fabric (NOT RAG)**.
  Structured-first layered retrieval around the World Model (direct → structured → relationships →
  memory); authorization enforced BEFORE any entity enters context; budgeted for the small model;
  semantic/vector + document knowledge are OPTIONAL seams (no embedding server / vector DB required).

- **Service API / IPC boundary + Core Alpha daemon** (`service.rs`, ADR-016) — `Request`/`Response`
  across all six surfaces (world/capabilities/policy/audit/components/intents); in-process + Unix-
  socket transports (length-prefixed JSON, std-only, no async/deps). Authorization stays inside the
  Core (fail-closed); the boundary only marshals. `aletheiad serve` = the long-running Core;
  `aletheiad demo` = a client over the boundary. The M1 scenario is reproduced as conformance tests
  THAT TRANSIT THE API (+ a socket round-trip) — apps/tests no longer call Core internals.

- **Aletheia HAL + first-class target matrix** (`kernel/src/hal.rs`, ADR-019) — the kernel is now
  written against an Aletheia-owned `Hal` trait (timer/privilege/exit), not a specific CPU. **AMD64/
  x86-64 and RISC-V are declared first-class targets**; aarch64 is the bootstrap/dev backend (VM-
  tested, still 11/11 green through the HAL). The x86-64/RISC-V backends are `cfg`-gated contracts —
  no untested bring-up code ships (ADR-010). The HAL imports no Linux/macOS/Darwin/POSIX.
- **Security hardening (22-agent adversarial-review pass)** — the wave was adversarially reviewed and
  the confirmed findings fixed: (CRITICAL) unauthenticated audit read + capability-token/decrypted-
  content leakage via the event log → tokens/content are no longer logged and `QueryAudit`/
  `ListApprovals` are `audit.read`-gated; (CRITICAL) ungated `Revoke` → now requires `capability.grant`
  authority (no owner-lockout); (HIGH) ungated `ResolveApproval` → only a principal who could perform
  the bound action may grant/deny; (HIGH) one-time owner bootstrap guard; (HIGH) socket frame cap
  (8 MiB) + per-connection read timeout (slow-loris/OOM); (HIGH) Context-Engine now enforces
  capability-before-inclusion for relationship EDGES, not just entities. Clippy `-D warnings` clean.

Deferred (next): both first-class HAL backends are now VM-tested and executed (see the x86-64 and
RISC-V delivered sections below). The shared **`kernel-core`** crate now holds not just the `Hal`
trait but the entire capability-secure **spine** and the **invariant selftest suite** as well — see
the kernel-core substrate section below (gap-register Issue 1). The remaining mechanical item is the
fuller cargo-**workspace crate split** of the hosted crate (SAD §4 — module boundaries + dependency
direction already match the crate list).

## Delivered (2026-07-21 — x86-64 bootable development image)

The first **bootable Aletheia disk image**: Aletheia boots as its own OS on **AMD64/x86-64** under
UEFI firmware, calls `ExitBootServices` to take the machine, brings up its own GDT/IDT + 8259 PIC +
8254 PIT, proves a timer IRQ actually fires, and re-proves the 11 capability-secure spine invariants
in x86-64 kernel space. Code in `kernel-x86_64/` (ADR-019 first-class AMD64 target, now executed —
contract-honest: written outside-in and boot-verified, not blind hardware code).

- **Boot model**: `x86_64-unknown-uefi` PE at `\EFI\BOOT\BOOTX64.EFI` on a GPT **EFI System
  Partition**; own COM1 serial + GOP framebuffer console; own `#[global_allocator]` (8 MiB static
  bump heap) + `#[panic_handler]` that stay valid after ExitBootServices. Post-exit the kernel
  **owns** the firmware page-table hierarchy: a physical frame allocator seeded from the UEFI
  memory map (`frames.rs`) + map/unmap over the live PML4 (`vm.rs`) — see the P5 note below. UEFI
  is the hardware/platform integration layer (ADR-019); the OS above it is entirely Aletheia-owned
  — no Linux/macOS/POSIX, no third-party OS framework.
- **Artifacts** (from `kernel-x86_64/scripts/build-image.sh`, macOS host, no mtools/xorriso/grub —
  only rust + hdiutil/diskutil + python3 + qemu-img): `build/aletheia-x86_64.img` (raw GPT/ESP) +
  `build/aletheia-x86_64.vmdk` (VMware) + `aletheia-x86_64.vmx`.
- **Verified**: boots in **QEMU 11 under OVMF/UEFI** (`edk2-x86_64-code.fd`) → full serial boot log
  + QEMU exit 33 = `[e2e] PASS`; `scripts/smoke-test.sh` is the automated boot gate (exit 33 +
  "[e2e] PASS"). QEMU-under-OVMF is the same UEFI firmware family VMware uses; VMware itself was not
  driven from the build host — attach the `.vmdk`/`.vmx` (UEFI firmware) to run it there.
- **Reuses** the SAME `spine.rs` / `selftest.rs` the aarch64 kernel compiles (shared via `#[path]`,
  no fork/copy); the aarch64 `-kernel` target is untouched and still green. The workspace/`kernel-core`
  crate split that unifies the one duplicated `Hal` trait is the mechanical follow-up.
- **Deferred (P5)**: own page tables/higher-half, TSS+IST double-fault stack, APIC/HPET + calibrated
  TSC, SMP, a real page-frame allocator, and the RISC-V first-class backend.

## Delivered (2026-07-21 — x86-64 ring-3 user-mode + PIT-driven preemption)

The x86-64 backend now proves the **same 13 user-mode invariants** the aarch64 EL0 suite does
(10 base ring-3 invariants + capability-secure IPC 11-13) — the first executed ring-3 privilege
boundary on the first-class AMD64 target. Code in
`kernel-x86_64/src/usermode.rs` (+ ring-3 GDT segments & TSS in `gdt.rs`, per-process address spaces
in `vm.rs`, DPL=3 syscall/#PF/timer vectors in `idt.rs`). `scripts/smoke-test.sh` now also gates on
`RING-3 BOUNDARY INVARIANTS HOLD`; QEMU exit 33 + `[e2e] PASS` still hold (13/13 green).

- **Real ring 3 (CPL 3)**: `iretq` drops to unprivileged code in USER-only pages; the one door back
  is an `int 0x80` DPL=3 gate authorized by the SAME `CapEngine` the deterministic pipeline uses. A
  save-first trap path stores the full register file into the running task's `TrapFrame`; a single
  `resume_frame`/`resume_return` primitive both starts a fresh task and resumes a preempted one.
- **Hardware isolation** (a ring-3 read of a supervisor-only page faults and is contained) and
  **per-process PML4 address spaces** (a page private to process A is unreachable from B at the same
  VA) — each process gets a private copy of the low PDPT with its 1 GiB user slot cleared, while the
  kernel/RAM/framebuffer identity mappings stay shared.
- **Preemptive multitasking**: the free-running 8254 **PIT IRQ0**, taken in ring 3, preempts two
  non-yielding tasks; the round-robin scheduler switches them and each resumes with its register
  state (progress counter) intact. Cooperative (`SYS_YIELD`) scheduling is proven too.
- **Two hard-won gotchas (documented in-code)**: (1) `x86_64-unknown-uefi` makes `extern "C"` the
  **Microsoft x64 ABI** — the trap assembly and its boundary fns are `extern "sysv64"` so the frame
  pointer arrives in RDI, not RCX. (2) QEMU/OVMF enforce the ring-3 code segment's **4 GiB limit** on
  the `iret` target, so the user region lives in the **1..2 GiB** range (below 4 GiB), not a high slot.
- **Deferred (P5)**: higher-half kernel, TSS+IST double-fault stack, APIC/HPET + calibrated TSC, SMP,
  and the RISC-V ring-3 backend.

## Delivered (2026-07-21 — RISC-V first-class backend, VM-tested)

The **second first-class target executed** (ADR-019): the Aletheia microkernel now boots on
**RISC-V / RV64GC** under QEMU `virt` and re-proves the 11 capability-secure spine invariants in
RISC-V kernel space — contract-honest (ADR-010: written outside-in and boot-verified, not blind
hardware code). Code in `kernel-riscv64/`.

- **Boot model**: QEMU loads **OpenSBI** (`-bios default`, M-mode) which hands off to our `-kernel`
  ELF entry (`_start`) in **S-mode** with `a0`=hartid, `a1`=DTB; the kernel parks secondary harts,
  sets its stack, clears BSS, installs an `stvec` trap vector, and runs. It drives the QEMU `virt`
  **NS16550A UART** directly for a robust console, and genuinely exercises the **S→M SBI boundary**
  (the RISC-V privilege-crossing interface) by calling the SBI Base extension — live boot shows
  `spec v3.0, impl=OpenSBI`. Timer is the S-mode `rdtime` (`time` CSR, 10 MHz on `virt`), shown
  advancing at boot.
- **Machine exit** is the **SiFive-test device** (MMIO `0x0010_0000`), NOT SBI SRST: SRST can only
  request a clean shutdown (exit 0) and so cannot signal a *failing* invariant, whereas the
  SiFive-test finisher encodes a code — `FINISHER_PASS` ⇒ QEMU exit 0 (e2e PASS), `(code<<16) |
  FINISHER_FAIL` ⇒ QEMU exit `code` (per-invariant failure / panic 101 / trap 102).
- **Reuses** the SAME `spine.rs` / `selftest.rs` the aarch64 and x86-64 kernels compile (shared via
  `#[path]`, no fork/copy); the aarch64 and x86-64 targets are untouched and still green. The one
  duplicated `Hal` trait across the three kernel crates is unified by the mechanical workspace/
  `kernel-core` split (the documented follow-up).
- **Verified**: `scripts/vm-e2e-riscv.sh` builds the kernel, boots it in QEMU riscv64 `virt`+OpenSBI,
  and asserts the SBI-boundary marker + `ALL 11 INVARIANTS HOLD` + the memory / virtual-memory /
  user-mode markers (below) + `[e2e] PASS` + **exit 0** (60s watchdog). Wired into CI as the
  `vm-e2e-riscv` job (GitHub + GitLab), alongside the aarch64 gate. `clippy -D warnings` clean.
- **Deferred (P5 follow-on)**: Sv48 + higher-half, PLIC/external interrupts, a frame-backed kernel
  heap, and SMP (secondary-hart bring-up). (Sv39 MMU + frame allocator + U-mode + per-process
  address spaces + timer preemption are now DELIVERED — see the RISC-V P5 parity section below.)

## Delivered (2026-07-21 — RISC-V P5 parity: Sv39 MMU, U-mode, per-process spaces, preemption, IPC)

The RISC-V backend now proves the **same memory-management + user-mode invariant suite** the aarch64
dev backend and the x86-64 image do — closing the cross-architecture process-isolation gap
(ARCHITECTURE-GAPS Issue 3): the RISC-V column of the capability matrix (physical allocator, MMU,
user mode, per-process address space, preemption) is no longer a gap. Contract-honest (ADR-010):
every module was written outside-in and boot-verified in QEMU; a wrong page table / bad trap faults
to `exit 102`, never a silent hang. All three new modules are riscv64-crate-only; the `#[path]`-shared
`spine.rs`/`selftest.rs` are untouched and the aarch64 + x86-64 targets stay green.

- **Physical page-frame allocator** (`frames.rs`) — intrusive LIFO free-list over RAM above the
  kernel image (QEMU `virt` DRAM base 0x8000_0000; `-m 128M`), identical in shape to the aarch64
  allocator. **7 memory invariants** proved live (distinct/aligned/in-range alloc, real R/W frame,
  misaligned-free rejected, exhaustion fail-closed, free revives allocation).
- **Sv39 virtual memory** (`vm.rs`) — 3-level Sv39 page tables built from frames: peripheral GiB as a
  Device gigapage leaf, RAM as 2 MiB megapage leaves; A/D set on every leaf (the RISC-V analogue of
  the aarch64 Access-Flag anti-hang move). The identity map is asserted by a **software page-table
  walk BEFORE `satp` is written**; then dynamic map/unmap is proved by writing through a fresh VA and
  observing the bytes in the mapped physical frame. **13 virtual-memory invariants** live.
- **U-mode + preemption + IPC** (`usermode.rs`) — drops the CPU to **U-mode** and reaches the OS
  through exactly one door: an `ecall` authorized by the **same `CapEngine`** the deterministic
  pipeline uses (`sscratch` holds the current task's save-first trap frame; `sepc`/`sstatus` carry
  the resume PC/privilege; `fence.i` makes freshly written user code fetchable). Proves **13 U-mode
  boundary invariants**: cap-gated syscall (deny w/o cap, allow ⇒ one event); hardware isolation (a
  U-mode load of a supervisor-only page faults, contained); **per-process `satp` address spaces** (A
  reaches its own page, B cannot reach A's VA); a **cooperative round-robin scheduler** (two tasks in
  distinct spaces run A,B,A,B… to exit, each echoing its own register magic through the shared code
  VA); **timer preemption** (the S-mode timer IRQ — armed via the **SBI TIME extension** + `sie.STIE`,
  cleared purely by re-arming, no interrupt-controller dance — preempts two non-yielding tasks and
  each resumes with its progress counter advanced); and **capability-secure kernel-mediated IPC**
  across distinct address spaces (send/recv fail-closed without the `ipc.send`/`ipc.recv` capability).
  A dead-timer escape (bounded spin countdown → self-exit) keeps the preemption test from ever
  hanging. `cargo run` now re-proves **11 spine + 7 memory + 13 virtual-memory + 13 user-mode**
  invariants + exit 0; `clippy -D warnings` clean.

## Delivered (2026-07-21 — P5 start: physical frame allocator + MMU virtual memory)

The first **real memory management** in kernel space, on the aarch64 dev backend — the layer that
turns "a capability-secure spine that boots" into an OS that can own physical memory and translate
addresses. Two bricks, each landed green and VM-asserted (ADR-010: written outside-in, boot-verified,
never blind hardware code; a wrong page table faults to `exit 102`, never a silent hang). Both new
modules (`kernel/src/frames.rs`, `kernel/src/vm.rs`) are aarch64-crate-only; the `#[path]`-shared
`spine.rs`/`selftest.rs` are untouched, and the x86-64 + RISC-V targets are re-verified green.

- **Physical page-frame allocator** (`frames.rs`) — an intrusive LIFO free-list over the RAM *above*
  the kernel image/stack/bump-heap (each free frame stores the next-free link in its own first 8
  bytes, so there is no side table). 4 KiB frames; `alloc` / `alloc_zeroed` (page-table shape) /
  `free`; fail-closed on exhaustion; rejects misaligned/out-of-range frees. **7 memory invariants**
  proved live in QEMU: real read/write frame, distinct/aligned/in-range allocation, misaligned-free
  rejected, exhaustion denies (fail-closed), freeing revives allocation.
- **MMU virtual memory** (`vm.rs`) — the first live address-translation regime. Builds an identity map
  from frame-allocator frames (peripheral GiB = Device, RAM = Normal; **Access Flag set in every
  descriptor**; 4 KiB granule, 39-bit VA, TTBR0). It **asserts the map with a software page-table walk
  BEFORE flipping `SCTLR.M`** (the single highest-leverage anti-hang move), enables translation with
  the MAIR/TCR/TTBR0 + invalidate/barrier dance, then proves **dynamic** virtual memory: map a fresh
  frame at a brand-new VA, write through the VA, observe the bytes land in the *different* physical
  frame the VA points at, unmap, confirm it no longer resolves. **13 virtual-memory invariants** green
  live in QEMU. `scripts/vm-e2e.sh` now asserts the memory + virtual-memory markers alongside the 11
  spine invariants.
- **Deferred (P5 follow-on)**: higher-half (TTBR1) kernel/user split, timer-driven (involuntary)
  preemption (GIC + generic-timer IRQ), a frame-backed kernel heap (the static bump heap stays
  load-bearing for now), and the x86-64/RISC-V MMU backends. (EL0 user-mode + the cap-gated syscall
  boundary + per-process address spaces + cooperative multitasking are now delivered — see the EL0
  section.)

## Delivered (2026-07-21 — P5: EL0 user-mode, per-process address spaces, preemptive multitasking)

The brick that makes the privilege boundary **real**. Until now every invariant was re-proved
*in kernel space* (EL1) — the benchmark's own honesty note says the measured loop "crosses no
privilege/address-space boundary," so isolation was logical, not hardware-enforced. This wave
drops the CPU to **EL0** (unprivileged), runs a genuinely less-privileged instruction stream in
its own EL0-only pages, and lets it reach the OS through *exactly one door*: an `svc` trap that
lands in the EL1 vector and is authorized by the **same `CapEngine`** the deterministic pipeline
uses. Code in `kernel/src/usermode.rs` (aarch64 dev backend; `#[path]`-shared `spine.rs` untouched,
x86-64 + RISC-V re-verified green). Contract-honest (ADR-010): written outside-in, boot-verified;
an *unexpected* fault stays fatal (`exit 102`) so a real bug can never masquerade as a pass.

- **Real EL0 excursions, one-shot** (not a scheduler — that is the follow-on multitasking brick).
  `enter_user` saves the kernel's callee-saved context and `eret`s to EL0 with a tiny
  position-independent stub in a fresh EL0-executable page (new `vm::USER_CODE`/`USER_DATA` AP
  bits = EL0 R/W, PXN); the stub issues one `svc` (or faults); the **0x400 vector** (`Lower EL,
  AArch64, Synchronous`, previously fatal) decodes `ESR_EL1.EC` — SVC `0x15` vs Data-Abort-lower-EL
  `0x24` — dispatches, then resumes the kernel via `enter_user_return`. The 0x200 EL1 `svc`
  bench fast-path is untouched. User pages are mapped into the **live** TTBR0 (`vm::active_root`).
- **Same authority mechanism at the boundary.** The syscall handler authorizes through
  `CapEngine::evaluate` against the process's granted capabilities — nothing ambient. Allow ⇒ the
  effect happens (an event recorded in the Store, actor = the EL0 process); Deny ⇒ −1, zero effect.
- **Per-process address spaces** (`vm::switch_address_space`, `vm::build_identity` per process):
  each EL0 process runs under its **own TTBR0 root**. A page private to process A, mapped at a
  virtual address, is **unreachable from process B at that same VA** — B takes a contained
  translation fault. Same VA, present in A's space and absent from B's ⇒ the spaces are genuinely
  separate, not one flat memory. The TTBR0 switch flushes the TLB (`tlbi vmalle1`); every process
  root replicates the kernel identity map, which is what makes switching TTBR0 mid-execution safe.
- **Cooperative multitasking — the first executed Aletheia context switch.** The whole trap path
  is unified on a **save-first** entry: `0x400` saves the full register file (x0–x30 + ELR + SPSR
  + SP_EL0) into the running task's `TrapFrame` *before* any clobber (`TPIDR_EL1` = current frame,
  `TPIDR_EL0` = save-time scratch), then decodes/dispatches; `resume_frame` restores a whole frame
  and `eret`s (the same primitive starts a fresh task and resumes a preempted one). Two EL0 tasks
  `yield` (`SYS_YIELD`) under a **round-robin scheduler**, running to completion in a deterministic
  `A,B,A,B,A,B,A,B`. Each task runs in **its own TTBR0 address space** (the scheduler switches
  address spaces per slice), and both tasks share ONE code VA — so a task carries a **register-magic
  in a callee-saved reg** it replays as the syscall arg every slice, and the kernel asserting each
  slice reports *its* task's magic proves BOTH that the entire register file (not just the PC) rode
  through each context switch AND that the per-slice address-space switch routed the shared VA to
  the right task's code.
- **Timer-driven (involuntary) preemption** — a real preemptive scheduler. A **GICv2** (distributor
  `0x0800_0000`, CPU interface `0x0801_0000`) + the **EL1 generic timer** (PPI INTID 30) deliver a
  periodic IRQ to vector `0x480`; the handler saves the preempted task's frame (the same save-first
  prologue as `svc`, shared via an asm macro), re-arms `CNTP_TVAL` **before** EOI (the timer
  condition is level-triggered — EOI-without-rearm would storm), and the scheduler round-robins. The
  two tasks run with IRQs unmasked (SPSR `0x340`) and **never yield** — a tight `add x19; subs x20;
  b.ne` loop — yet the timer preempts them and each resumes with its counter (`x19`) advanced,
  proving state survives an *involuntary* switch. Contract-honest anti-hang: the loop is **bounded**
  (`x20` countdown) so a timer that never fires makes the task self-exit → a clean failure, never a
  spin; and `-machine virt,gic-version=2` is pinned so a GICv3 can't silently swallow the MMIO CPU
  interface. The GIC/timer are torn down after the test so the benchmark is unperturbed.
- **13 EL0-boundary invariants proved live in QEMU** (exit `80+i` on failure): (1) an EL0 process
  with **no capability** is denied at the boundary and leaves zero effect; (2) a **capability-granted**
  EL0 process is authorized via the same `CapEngine` and records exactly one event; (3) **hardware
  address-space isolation** — an EL0 read of kernel memory takes a permission Data Abort that is
  contained (proving EL0 truly cannot touch EL1 memory, not just "shouldn't"); (4) process A reaches
  a page in **its own** address space; (5) process B **cannot** reach A's page at the same VA
  (per-process isolation); (6) the **round-robin scheduler** runs two tasks (each in its own space)
  A,B,A,B,… to completion; (7) each task **resumes with its own register magic** at the shared VA
  (full context + the per-slice address-space switch); (8) the two scheduled tasks occupy **distinct
  TTBR0 address spaces**; (9) the **generic-timer IRQ preempts** two non-yielding tasks and the
  scheduler round-robins both; (10) each task's **register counter advances across preemptions**
  (state preserved under an involuntary switch); (11) **capability-secure IPC** — a message is
  delivered kernel-mediated across distinct address spaces; (12) an IPC send **without** the
  `ipc.send` capability is denied, endpoint untouched (fail-closed); (13) an IPC recv **without** the
  `ipc.recv` capability is denied, the queued message intact (fail-closed). `cargo run` now boots and
  re-proves **11 spine + 7 memory + 13 virtual-memory + 13 user-mode** invariants + exit 0.
- **Deferred (P5 follow-on)**: higher-half (TTBR1) kernel/user split, a frame-backed kernel heap
  (the static bump heap stays load-bearing for now), SMP (secondary-hart bring-up), and the
  x86-64/RISC-V EL0/preemption backends.

## Delivered (2026-07-21 — kernel-core substrate: shared spine + hosted arch-independent invariants)

The first real slice of gap-register **Issue 1** (architecture-independent `kernel-core`): the
capability-secure **spine** (`spine.rs` — content-addressed store, unforgeable capability engine,
intent→action pipeline, secure IPC) and the **invariant selftest suite** (`selftest.rs` — the 11 M1
acceptance criteria) are no longer `#[path]`-copied into each target crate. They now live **once** in
`kernel-core/` as real library modules that all three targets (`kernel/` aarch64, `kernel-x86_64/`,
`kernel-riscv64/`) depend on. This directly satisfies Issue 1 criterion #1 ("core kernel abstractions
are not duplicated across architecture crates") — previously only the `Hal` trait was shared; the
spine itself was textually included.

- **One source of truth, three backends.** Each target keeps only what is genuinely
  architecture-specific — its `hal.rs` backend `impl Hal` and its own console (`kprintln!`). The
  spine has zero architecture dependency (pure `no_std` + `alloc`), so it compiles identically for
  all three CPUs from the same file.
- **Console decoupling.** `selftest::run` no longer hard-codes a console macro; it reports each check
  through a caller-supplied `report(index, passed, name)` logger. Each kernel passes a `kprintln!`
  closure that prints the familiar `  [pass NN] name` lines; the invariant logic and its naming are
  defined exactly once in `kernel-core`.
- **Arch-independent invariants now run in HOSTED tests** (Issue 1 acceptance criterion #5): because
  the spine is arch-independent, `kernel-core/tests/invariants.rs` proves the whole suite on the host
  in a fast `cargo test` (13 tests, no QEMU) — running the SAME `selftest::run()` the three kernels
  boot, plus granular named per-invariant tests. This complements (does not replace) the per-target
  QEMU VM gates.
- **Capability transfer through IPC + bounded queues** (gap-register Issue 2) now live in the shared
  spine, so all three targets gain them at once: `Channel::send_transfer` authorizes a send AND
  delegates a capability from sender to recipient, **attenuated** by the same rules as
  `CapEngine::delegate` (a transfer can never amplify); the recipient receives a real, auditable,
  revocable registry token in `Message.cap`. `Channel::bounded` refuses a send to a full inbox
  fail-closed. All-or-nothing: an unauthorized send, an amplifying grant, or a full queue enqueues
  nothing and mints no token. Proved in `kernel-core/tests/invariants.rs`.
- **Verified green on every gate.** After the extraction + IPC extension: hosted `kernel-core` 17/17;
  **all three VM gates still pass** — aarch64 (`vm-e2e.sh`, exit 0), RISC-V (`vm-e2e-riscv.sh`, exit
  0), x86-64 (`smoke-test.sh`, exit 33) — re-proving 11 spine + memory + virtual-memory + user-mode
  invariants from the shared source. `clippy -D warnings` clean.
- **Deferred (Issue 1 follow-on):** extracting the remaining arch-independent primitives the register
  lists (task / process / address-space / scheduler / interrupt abstractions) into `kernel-core` so
  the per-target `usermode.rs`/`vm.rs`/`frames.rs` implement shared interfaces rather than parallel
  bespoke code; and the fuller cargo-workspace split.

## Delivered (2026-07-22 — P6 substrate: IPC tail, scheduler abstraction, security suite, traceability gate)

Four contract-honest bricks advancing the gap register's top-priority P6 items, plus the phased
architecture text for the hardware-bound issues. Every code brick is TDD'd, hosted-proved, and
clippy-clean; the aarch64 VM gate stays green (exit 0, all invariant markers). `kernel-core` hosted
suite grows **17 → 41** (6 suites).

- **IPC substrate tail** (gap Issue 2, ADR-020) — the IPC layer is consolidated into one
  arch-independent `kernel-core::ipc` module (re-exported from `spine` so all three targets and the
  selftest suite are unaffected) and extended with the primitives a real microkernel IPC needs beyond
  synchronous send + bounded queues + attenuated capability transfer: **asynchronous notifications**
  (coalescing seL4-style badge, capability-gated signal), **deadline/timeout-aware receive** (a
  message past its deadline is dropped, never delivered late — fail-closed), **cancellation** of an
  undelivered message by id, and **tracing + deterministic replay** (`replay()` reconstructs the exact
  delivered sequence from the trace alone). 9 new hosted tests.
- **Adversarial security-behaviour suite** (gap Issue 11) — permanent regressions that attack the
  capability engine as an adversary would: **confused deputy** (no ambient authority — `evaluate`
  consults only offered tokens), **capability laundering** (cannot mint fresh authority from a
  revoked/expired parent, nor launder a broader scope through a transfer), **TOCTOU / stale
  capability** (revocation is immediate — no cached authorization window), and **cross-principal
  leakage** (scope confinement + action-wildcard does not over-match a neighbouring namespace).
  9 hosted tests; all green (the engine holds).
- **Machine-checkable traceability gate** (gap Issue 12) — `docs/TRACEABILITY.md` is a machine-readable
  matrix of **64 requirements** (56 delivered, 5 partial, 3 deferred as of 2026-08-02; it was 45 —
  34/2/9 — when the gate was introduced), each mapping
  ReqID → ADR → implementation → test → VM gate → status. `scripts/check-traceability.sh` (pure bash,
  no new CI dep) FAILS the build if any delivered/partial requirement lacks Implementation+Test
  evidence that exists in the tree, or carries an unknown status; deferred work is explicitly
  distinguished and never counted as delivered. Wired as the `traceability` job in both GitHub Actions
  and GitLab CI; negative-tested against three bad fixtures.
- **Arch-independent scheduler + task abstraction** (gap Issue 1, first extraction beyond the shared
  spine) — `kernel-core::sched`: `TaskId`, a `TaskState` lifecycle, a `TaskContext` backend seam
  (save/restore stays arch-specific), and a `RoundRobin` scheduler (FIFO fairness + block/unblock/
  finish transitions), lifting the scheduling **policy** the three targets' `usermode.rs` each
  hand-roll into one place, proved on the host (6 tests). Wiring each target's asm context switch to
  drive it is the documented follow-on (its asm is unchanged and still VM-gated).
- **Component signature verification** (gap Issue 7 / ADR-025 Phase 1, secure boot's hosted first
  slice) — a component is a content-addressed `Application` entity; its provenance is a detached
  HMAC-SHA256 signature over its content hash under a trusted key (`aletheia/src/provenance.rs`,
  `crypto::hmac_sha256` built on the existing `sha2`, RFC-4231-validated, no new dependency). Under an
  opt-in secure policy (`set_require_signed_components`, default OFF for back-compat), `run_installed`
  refuses a component whose stored signature is missing or does not verify (fail closed), and
  `install_signed_component` refuses an untrusted/tampered signature at install. 5 hosted tests +
  crypto/provenance unit tests; the aletheia hosted suite grows to **77 passed** with zero regressions.
  Asymmetric keys, a key hierarchy, measured boot, and rollback protection remain ADR-025 Phase 2–3.
- **Phased-plan ADRs 020–026** — contract-honest architecture text for the hardware-bound issues
  (SMP, AI execution substrate, device/driver model, persistent storage, secure boot, fault recovery)
  so no deferred requirement implies code that does not exist; each names its hosted-testable first
  slice where one exists.

## Delivered (2026-07-24 — REQ-SMP-005: CPU affinity + cross-core migration + lock-hierarchy audit, ADR-021 Phase 4 / ADR-028)

The REQ-SMP-001 tail — **REQ-SMP-001 now `delivered`**. `kernel_core::smpsched` gains **CPU affinity**
(an `AffinityMask` INLINE in every run-queue element — placement + steal both honor it, never a second
locked side-table, so the "one queue lock per CPU" discipline holds) and **cross-core migration** (a
task enqueued on one CPU, dispatched by another via a steal, resumed on the thief through the
`kernel_core::sched::TaskContext` seam). The **lock-hierarchy + atomic-ordering audit** is **ADR-028**:
a total lock order (a forest ⇒ deadlock-free), a per-atomic-site ordering justification, and a
per-instance debug tripwire asserting "≤1 run-queue lock per CPU" — live under the mixed-affinity
contention suite AND every `-smp 4` VM gate, with a `#[should_panic]` proof that it is armed.

- **Host-proved** (`kernel-core/tests/smpsched.rs`, 11 tests): affinity honored, FIFO preserved among
  eligible tasks, affine placement balances, migration resumes on the thief via the seam, exactly-once
  under 4-thread MIXED-affinity contention, tripwire fires on a deliberate nest.
- **VM-gated on ALL THREE targets**: SMP invariants 19-21 (affinity honored, cross-core migration by
  stealing, resume via the `TaskContext` seam — a minimal GPR restore: aarch64/x86 `mov`, RISC-V `mv`),
  driven deterministically by the boot core over a private `SmpSched` (the invariant-12 first-steal
  doctrine — no race). Each target now reports **ALL 22 SMP INVARIANTS HOLD**.
- **Honesty:** proves the migration MECHANISM + resume seam on real cores; preemptive *timing* stays the
  per-target usermode preemption suites; NO `usermode.rs` rewire (the documented `sched.rs` follow-on).
  Affinity + stealing can starve a task pinned to a permanently-busy CPU — inherent to affinity; callers
  must supply satisfiable masks.

Gates: aarch64 vm-e2e PASS (80 invariants incl. SMP 22) · riscv64 PASS (75 incl. SMP 22) · x86-64
smoke PASS (68 incl. SMP 22) · e2e-all 3/3 · conformance 3/3 · kernel-core hosted **90** · clippy
`-D warnings` + fmt clean. Traceability: **57 reqs — 50 delivered / 4 partial / 3 deferred**.

## Delivered (2026-07-24 — REQ-SMP-004: cross-core TLB shootdown, ADR-021 Phase 3)

The SMP correctness cliff the audit flags in THREE gap docs (GAPS4 **ALET-P1-005**, GAP3 §4.2,
GAPS2 #4): once a second CPU exists, one CPU editing a page table (unmap / remap to a new frame) in
an address space another CPU has active leaves that CPU using a STALE TLB translation — a
cross-address-space use-after-free. The contract: a CPU about to **reclaim** a frame must not proceed
until every CPU that could hold a stale translation has **completed** its local invalidation.

- **Arch-independent coordination — `kernel_core::shootdown::TlbShootdown`** (defined ONCE, Issue 1):
  an all-acknowledged barrier. `request(targets, inv, keep_waiting)` posts an `Invalidation` to each
  target and blocks until every one has drained it, run its local invalidation via the `service`
  callback, AND acknowledged — the ack is bumped strictly AFTER the invalidation, so a reached
  watermark proves the work happened-before the reclaim. An offline / unresponsive / failed CPU makes
  `request` hit its deadline and return `false` → the reclaiming CPU refuses to reclaim (fail closed),
  never hangs. Alloc-free `request` past construction.
- **Native mechanism per target** (NOT forced-uniform — a hardware broadcast exists on only one):
  **aarch64** — the initiator's `tlbi vaae1is` broadcasts across the inner-shareable domain in
  hardware (the real invalidation); the barrier proves per-core acknowledgement. **x86-64** — NO
  hardware broadcast, so each AP runs its own core-local `invlpg` via the service callback (the
  genuine software shootdown; `map_kernel_frame` clears/restores CR0.WP so a PTE write into OVMF's
  read-only live page tables does not `#PF`). **RISC-V** — the SBI RFENCE `remote_sfence_vma` firmware
  fence + each hart's own `sfence.vma` through the barrier.
- **19 SMP invariants per target** (was 16; +3 = shootdown invariants 16-18): each Phase 6 maps a
  fresh VA to frame A in the shared kernel root, every secondary primes its TLB, the initiator remaps
  the VA to frame B and shoots down, and every secondary re-reads and observes B coherently, all
  `-smp 4` VM-gated.
- **Honesty (ADR-021 Phase 3):** the VM gates prove the mechanism + barrier RAN on real cores and the
  initiator waited for every acknowledgement — NOT that QEMU exhibits a stale entry (TCG's softmmu TLB
  is a performance cache, not a faithful retention model). The *discriminating* stale-vs-fresh proof
  (a broken barrier = a genuine failure) is the deterministic host-thread test
  `kernel-core/tests/shootdown.rs` (5 tests: barrier ordering, the use-after-free scenario,
  no-lost-request under concurrent requesters, fail-visible on an unresponsive target, FIFO+count).
- **Honesty (open at the time of this slice; delivered by REQ-SMP-005 above):** preemptive cross-core
  *task migration* (a stolen task resuming on the thief CPU through the `TaskContext` seam), CPU
  affinity, and the lock-hierarchy / atomic-ordering audit.

Gates: aarch64 vm-e2e PASS (77 invariants incl. SMP 19) · riscv64 PASS (72 incl. SMP 19) · x86-64
smoke PASS (65 incl. SMP 19) · e2e-all 3/3 · conformance 3/3 · kernel-core hosted **84** · clippy
`-D warnings` + fmt clean. Traceability: **56 reqs — 48 delivered / 5 partial / 3 deferred**.

## Delivered (2026-07-24 — REQ-SMP-003: per-CPU run queues + work stealing, ADR-021 Phase 2 policy)

The scheduler shape that scales past one core. One global run queue serializes every scheduling
decision behind one lock; `kernel-core/src/smpsched.rs` (`SmpSched`) gives each CPU its OWN queue —
dispatch is **local-first** (the common case contends with nobody) and an idle CPU **steals from
the most-loaded** victim. **Lock discipline (load-bearing):** never two queue locks at once — a
steal snapshots loads via brief single locks, then locks exactly ONE victim; with at most one queue
lock held per CPU at any instant, no lock-order cycle can exist. The steal path is **alloc-free**
past construction (kernel CPUs spin on it while waiting for stragglers, and the bare-metal bump
allocators never reclaim).

- **Host-proved under real threads** (`kernel-core/tests/smpsched.rs`, 5 tests, progress-gated):
  exactly-once dispatch under 4-thread contention with everything seeded on one queue (none lost,
  none duplicated; the other CPUs progress only by stealing), local-first, steal liveness + victim
  attribution, most-loaded-victim preference, least-loaded placement balance. `kernel-core` hosted
  suite grows to **79 passed** (13 suites).
- **VM-gated on REAL cores** (`kernel/src/smp.rs` phase 5 — aarch64 suite now **16 invariants** at
  `-smp 4`, total 74): core 0 seeds all 64 work items on CPU 1's queue alone, so core 0 and CPUs
  2..3 can progress ONLY by stealing; the gate asserts the phase completes on every core, every
  task is dispatched EXACTLY once across cores, and stealing drains the unbalanced queue. The
  steal invariant is structural, not a race: core 0 performs one uncontended steal before opening
  the phase to the other cores.
- **RISC-V + x86-64 parity (same day):** `kernel-riscv64/src/smp.rs` and `kernel-x86_64/src/smp.rs`
  run the identical phase (same seed-on-one-secondary shape, same three invariants; the RISC-V
  scheduler is sized MAX_CPUS and indexed by hartid because OpenSBI's boot-hart lottery makes ids
  arbitrary — the seed queue is the lowest started secondary hart). ALL THREE targets now gate 16
  SMP invariants at `-smp 4`.
- **Honesty:** this is the scheduling *policy* proved on real cores dispatching kernel work items
  (what runs where). Preemptive cross-core *task migration* (a stolen EL0 task resuming on the
  thief CPU through the `TaskContext` seam), TLB shootdown, and the lock-hierarchy/atomic-ordering
  audit stay open under **REQ-SMP-001 (partial)**.

Gates: aarch64 vm-e2e PASS (74 invariants incl. SMP 16) · riscv64 PASS (69 incl. SMP 16) · x86-64
smoke PASS (62 incl. SMP 16) · e2e-all 3/3 · conformance 3/3 · kernel-core hosted 79 · clippy
`-D warnings` + fmt clean. Traceability: **55 reqs — 47 delivered / 5 partial / 3 deferred**.

## Delivered (2026-07-24 — REQ-SMP-002: SMP secondary bring-up + cross-core concurrency substrate, VM-gated at -smp 4)

The SMP cliff (gap #4, ADR-021 Phase 1) is broken: Aletheia now **boots and runs on multiple real
CPUs**. `kernel/src/smp.rs` (+ `boot.s::_secondary_start`) powers on every present secondary via the
PSCI `CPU_ON` firmware call (HVC conduit), gives each a private 16 KiB stack and per-CPU identity
(`TPIDR_EL1`), and enables its MMU over the SAME kernel page tables core 0 built — then proves the
cross-core substrate with **13 VM-gated invariants** (`scripts/vm-e2e.sh` now boots `-smp 4` and
asserts `ALL 13 SMP INVARIANTS HOLD`; with `-smp 1` the suite skips green like virtio-with-no-disk,
and the pinned gate makes a silent skip impossible).

- **Bring-up + identity (inv 1-3):** PSCI accepts CPU_ON for 3 secondaries; all come online with
  translation on; each core's MPIDR affinity + TPIDR_EL1 are distinct (per-CPU data works).
- **Cross-core memory model (inv 4-7):** 4 cores hammer one counter — the total is EXACT (real
  atomicity, no lost increments); a release/acquire mailbox publishes a payload every core observes
  and answers with a per-CPU transform. The kernel bump allocator moved from load-then-store to
  **CAS** (`heap.rs`) — the first removed single-core assumption (two cores could previously be
  handed the same bytes).
- **ADR-027 on REAL cores (inv 8-11):** the `with_authorization` atomic authorize+execute primitive
  — until now proved only under host threads — runs under the kernel's first **`SpinLock`** while 3
  secondaries commit and core 0 revokes: commits flow pre-revoke (progress-gated, never a fixed
  spin), the revoke linearizes inside the lock hold, **ZERO commits land after it**, and all 64
  post-revoke attempts per core fail closed. GAPS2 #9's mechanism is now SMP-proved.
- **IPI (inv 12):** a GICv2 **SGI** from core 0 is delivered to and acknowledged on every
  secondary's banked CPU interface (polled IAR with PSTATE.I masked — the secondaries never touch
  the core-0-owned vector table). Distributor state is restored after.
- **Stability (inv 13):** all secondaries park in WFE; online mask + counters unchanged.
- **Concurrency rules (load-bearing):** secondaries never print (PL011 unserialized); every engine
  access sits under the one SpinLock; liveness waits are progress-gated with CNTPCT deadlines.

**RISC-V parity (same day):** `kernel-riscv64/src/smp.rs` + `boot.s::_secondary_start` replicate
the full suite through **SBI HSM `hart_start`** — with a boot-hart-lottery-safe atomic first-comer
claim in `_start` (OpenSBI may pick ANY hart as boot hart; `BOOT_HART` is recorded in `.data`
before BSS zeroing) — per-hart `tp` identity, Sv39 enable over the shared tables, and the SBI
**IPI** (`send_ipi` → polled `sip.SSIP`, SIE masked). Same 13 invariants, VM-gated by
`scripts/vm-e2e-riscv.sh` at `-smp 4` (66 total riscv invariants). The `SpinLock` was extracted to
**`kernel-core/src/sync.rs`** — defined ONCE (Issue 1), host-proved under real threads
(`kernel-core/tests/sync.rs`: exclusion exactness + no torn publication), used by both targets.

**x86-64 parity (2026-07-24, same wave):** `kernel-x86_64/src/smp.rs` closes the last bring-up leg
— the hardest one, because x86 has NO firmware bring-up service after `ExitBootServices`; the OS
itself is the protocol. The ACPI **MADT** (RSDP stashed from the UEFI config table pre-exit)
enumerates the APs; the LAPIC **INIT-SIPI-SIPI** sequence wakes each into a 16-bit real-mode
**trampoline** at physical `0x8000` (a `global_asm!` blob copied + parameterized at runtime — all
addresses are assembler-time constants against the fixed base, so no relocation; its PTE is made
present/writable/executable via a manual CR3 walk because OVMF may cover the low megabyte with a
2 MiB NX leaf) that climbs real→long mode in ONE hop by cloning the BSP's CR4/CR3/EFER/CR0 over
the SHARED page tables. Per-CPU identity = `IA32_GS_BASE` + LAPIC ID (the TPIDR/tp twin); the IPI
is a fixed-vector LAPIC interrupt taken through a dedicated **AP IDT** (handler tags the CPU via
GS_BASE and writes EOI — the BSP's IDT stays untouched for the ring-3 suite that runs later).
Ordering is load-bearing: the SMP suite runs BEFORE `usermode::selftest`, which repoints IRQ0 at
its own context-switch entry and leaves IF=0 — running after would strand the PIT deadline clock.
Same 13 invariants, gated by `kernel-x86_64/scripts/smoke-test.sh` at `-smp 4` (x86-64 total now
59 invariants); `-smp 1` skips green (verified). APs launch strictly sequentially (each reads its
stack + index from the trampoline data block before signalling online), never print, and park in
`cli; hlt` before the ring-3 suite touches the machine.

Gates: aarch64 vm-e2e PASS (71 invariants incl. SMP 13) · riscv64 vm-e2e PASS (66 incl. SMP 13) ·
x86-64 smoke PASS (59 incl. SMP 13, `-smp 4`) · conformance 3/3 PASS · kernel-core hosted 74 ·
clippy/fmt clean. **Honesty:** this is ADR-021 Phase 1 + the concurrency-substrate slice, now on
ALL THREE targets — per-CPU run queues/work stealing (Phase 2), TLB shootdown, and the
lock-hierarchy/atomic-ordering audit stay open under **REQ-SMP-001 (partial)**. Traceability:
**54 reqs — 46 delivered / 5 partial / 3 deferred**.

## Delivered (2026-07-22 — REQ-CAP-006: capability concurrency semantics, the SMP prerequisite)

The gate the audit itself put **before** SMP (GAPS2 #9 "the capability model needs a formal
concurrency specification before SMP"; #9 precedes #4). Until now the capability engine's safety
rested on an implicit single-core assumption: Rust's borrow checker serializes a `&self` `evaluate`
against a `&mut self` `revoke`. SMP breaks that — two cores behind one lock can interleave the
pipeline's *authorize* and *execute* steps, so an effect acts on a capability revoked in the gap
(a classic time-of-check/time-of-use bug). ADR-027 specifies the guarantee and `kernel-core` now
implements + proves it, contract-honest.

- **Specified (ADR-027):** authorization and the effect it authorizes commit inside **one critical
  section**; an effect executes only if its capability is live at that point; revocation is immediate
  and permanent (no cached authorization, no authority resurrection). **Option A** (single lock —
  re-check inside the critical section, no epochs) is chosen and built; **Option B** (generation/epoch
  tokens for a future lock-free authorize) is documented but deliberately NOT built (ADR-010 + YAGNI).
- **Implemented (additive):** `CapEngine::with_authorization(action, target, offered, commit)` runs
  the check and, iff `Allow`, the `commit` closure within one `&self` call — because `revoke` needs
  `&mut self`, under the engine lock no revoke can linearize between check and effect, making the
  TOCTOU gap **unrepresentable**. `CapEngine::authorize` is the read-only variant that also names
  *which* token matched (an `Authorization`); `evaluate`/`revoke`/`mint`/`delegate` signatures are
  **untouched** (both authorization paths route through one private `test_token` matcher, so they
  cannot drift). The `now` clock is fixed at construction, so it is not shared-mutable under concurrency.
- **Proved under REAL threads (`kernel-core/tests/cap_concurrency.rs`, 5 tests):** the naive
  `check(); … ; act();` pattern is stale by construction; then an `RwLock`-guarded engine is hammered
  by committer threads vs. a revoker (progress-gated so the Allow path is genuinely exercised
  regardless of thread-wakeup order — a fixed spin races the scheduler) and asserts the disciplined
  primitive **never** commits under a revoked capability and that **revocation is permanent** (a
  committer that observes revoke-completed can never then see `Allow`). `kernel-core` hosted suite
  **72 passed**; aarch64 VM gate still green (`[e2e] PASS`, exit 0); `clippy -D warnings` + `fmt` clean.

**Honesty (advisor):** this proves the **mechanism** under host threads — it does **not** prove an
SMP-safe kernel (none exists). Wiring `with_authorization` into each target's real trap/IPC path, plus
the TLB-shootdown / atomic-ordering audit, is the SMP integration still **deferred** under
**REQ-SMP-001** (gap #4, ADR-021); REQ-CAP-006 is the prerequisite spec that unblocks it. Traceability
green (**53 reqs — 45 delivered / 4 partial / 4 deferred**).

## Delivered (2026-07-22 — REQ-DRV-003: virtio-blk driver — the FIRST real hardware driver, VM-gated)

The named next slice of ADR-023, executed. Until now the `kernel_core::storage::BlockDevice` seam was
only ever backed by an in-memory `MemBlockDevice`; this brick implements it over a **genuine emulated
block device** — a **virtio-blk** driver over **modern (v2) virtio-mmio** on the aarch64 QEMU `virt`
dev backend (`kernel/src/virtio.rs`) — so the write-ahead journal (REQ-STOR-002) now runs over real
emulated storage. This closes gap-register Issue 5's "no concrete driver" hole. Contract-honest
(ADR-010): written outside-in, boot-verified; a wrong ring layout faults/hangs into the 60s VM
watchdog, never a silent pass.

- **Discovery + modern handshake.** Scans the 32 virtio-mmio slots for magic (`0x74726976`) +
  `DeviceID==2`; init = reset → `ACKNOWLEDGE|DRIVER` → feature negotiation (accept **only**
  `VIRTIO_F_VERSION_1` + `VIRTIO_BLK_F_FLUSH` when offered) → `FEATURES_OK` read-back → queue-0 setup
  → `DRIVER_OK`. **Fails closed** on a legacy (v1) transport — no silent wrong-mode driver.
- **Split virtqueue + request protocol.** A 3-descriptor chain (header / data / status) with `dsb`
  barriers around `QueueNotify` and the used-ring poll (the classic virtqueue ordering trap), a
  bounded poll (anti-hang), and descriptors carrying **physical** addresses (VA==PA under the identity
  map). `read_block`/`write_block`/`flush` each issue one request. The 4 KiB `BlockDevice` block maps
  to **8** 512-byte virtio sectors (sector = idx × 8).
- **No ambient authority (REQ-DRV-002 over the real device).** The driver holds only the frames it
  allocated; wrapped in `DeviceGuard`, every block op is authorized by the SAME `CapEngine` — proved
  live: no capability ⇒ no bytes move; a write capability's bytes land and read back.
- **5 VM-gated invariants** (`scripts/vm-e2e.sh`, exit 0, `ALL 5 VIRTIO-BLK INVARIANTS HOLD`):
  (1) device discovered + initialized; (2) capacity read matches the attached 1 MiB image (256 × 4 KiB
  blocks); (3) write→read-back virtqueue round-trip returns the written bytes; (4) `Journal` commit +
  a FRESH `recover` reproduce state from the device bytes alone (crash-consistency over real storage —
  the ADR-023 payoff); (5) capability-gated I/O via `DeviceGuard`. **Graceful skip** under bare
  `cargo run` (`[virtio] no device (skipped)`) so the disk-less runner stays green; the gate attaches
  the disk and forces modern mmio (`-global virtio-mmio.force-legacy=false` — QEMU defaults the mmio
  transport to legacy v1). `clippy` clean.
- **Two gotchas (documented in-code):** (1) QEMU's `virtio-blk-device` on `virt` presents **legacy
  (v1)** virtio-mmio unless `virtio-mmio.force-legacy=false`; the driver's version check caught this
  as a fail-closed init error before any I/O. (2) descriptor addresses are physical DMA targets — the
  identity map makes VA==PA, so frame-allocated ring/buffer addresses are handed to the device raw.
- **Deferred (umbrella REQ-DRV-001):** hotplug, DMA/IOMMU confinement, driver-crash isolation +
  supervisor restart (ADR-023 Phase 3), and the RISC-V/x86-64 virtio backends (the aarch64 driver is
  the reference — the same cross-target spread pattern as blocking-IPC/priority-inheritance).
  Traceability green: **52 requirements — 44 delivered / 4 partial / 4 deferred**.

## Delivered (2026-07-22 — REQ-DRV-002: capability-authorized device access)

Fourth P7 brick — the capability model extended to hardware (no ambient device authority).
`kernel-core/src/device.rs` `DeviceGuard` wraps any `BlockDevice` and gates every read/write/flush on
the SAME `CapEngine`, so device I/O is authorized exactly like an entity write or an IPC send. Proved
over the **real** `MemBlockDevice` — deny/allow decides actual bytes, not a registry boolean:

- **No capability ⇒ no I/O** (`tests/device.rs`): read/write/flush all `Denied`, nothing moves.
- **Read-only capability reads but cannot write** (attenuation): a `dev.blk.read` holder reads the
  real block yet a write is `Denied` and the device is confirmed unchanged.
- **Write capability's bytes actually land** and read back; a `dev.blk.*` wildcard authorizes both.

`kernel-core` **67 passed**; aarch64 VM gate green (compiles no_std); `clippy -D warnings` clean.
**Honesty (advisor):** NEW req **REQ-DRV-002** delivered; the umbrella **REQ-DRV-001** (device
discovery, a real hardware driver, hotplug, DMA/IOMMU, restart) stays `partial` — the concrete
**virtio-blk driver, which will implement this very `BlockDevice` trait**, is the named next slice,
deferred (ADR-023, hardware-bound, ADR-010). A precise **virtio-blk implementation plan** (virtio-mmio
discovery → feature negotiation → split-virtqueue setup → request protocol → `BlockDevice` impl → QEMU
wiring → VM-gated invariants) is now written into ADR-023 so a fresh-context session executes it in one
focused pass. Traceability green (51 reqs — 43 delivered / 4 partial / 4 deferred).

## Delivered (2026-07-22 — REQ-CONF-001: cross-architecture semantic conformance gate, GAPS2 #2)

Third P7 brick — the consolidation the audit called the #1 systemic risk: *silent behavioral
divergence* between the three CPU backends. `scripts/conformance.sh` boots all three targets and
asserts each proves the **same core semantic contract** — 10 arch-neutral named behaviors
(capability-secure cross-AS IPC, the grant-table's cap-gate/zero-copy/revoke, blocking IPC's
block/wake/resume, and priority inheritance's inversion-avoidance/service/receive).

- **Spec'd on named behaviors, not invariant counts** (advisor): the contract substrings deliberately
  omit the privilege term (el0/u-mode/ring3), the address-space term (TTBR0/satp/PML4), and the trap
  term (svc/ecall/int 0x80), so a genuine behavior matches on every arch. Per-arch invariants
  (x86-64's 46 total vs aarch64/RISC-V's 53 — long mode can't do the MMU-off→on flip) are **extensions**
  reported informationally, never failures. x86-64 is SKIPPED (never silently passed) where the host
  lacks the image toolchain.
- **Result:** all three targets PASS all 10 core behaviors — the "one coherent kernel, three backends"
  thesis is now machine-checked, not asserted. Traceability green (50 reqs — 42 delivered / 3 partial
  / 5 deferred). Follow-on: wire as a CI job alongside `e2e-all`; extend the contract as new
  cross-target behaviors land.

## Delivered (2026-07-22 — REQ-STOR-002: crash-consistent journaled block store)

Second P7 brick (persistent storage, ADR-024). A general-purpose OS needs storage that survives power
loss without corruption. `kernel-core/src/storage.rs` delivers the arch-independent middle of that
stack: a **write-ahead journal** over an abstract `BlockDevice` seam (a real virtio-blk driver,
REQ-DRV-001, will implement the same trait). `alloc`-only — the journal core is kernel-portable.

- **Atomic commit protocol:** write the redo data to the journal area → flush → write a **checksummed
  commit record** → flush (*the atomic pivot*) → apply to home blocks → flush.
- **Recovery is binary:** if the commit record's magic is absent or its checksum (over header **and**
  journal payload) fails, the transaction is uncommitted and nothing is applied; otherwise the journal
  is replayed idempotently. So for **every** crash point recovery yields the pre- or the fully-applied
  state — never torn.
- **Proved (`kernel-core/tests/storage.rs`, 5 tests):** the load-bearing **crash-at-every-prefix
  sweep** (capture a 2-block txn's ordered writes; for every prefix K, materialize the device with
  only the first K writes, recover, assert both home blocks are pre OR fully post — never torn); a
  torn commit record → rolled back; a torn journal payload → rolled back (checksum load-bearing,
  corruption surfaced not swallowed); full-commit replay is idempotent; a blank device recovers to
  nothing (fail closed). `kernel-core` hosted suite **63 passed**; aarch64 VM gate still green (the
  new module compiles no_std); `clippy -D warnings` clean.

**Honesty (advisor):** NEW requirement **REQ-STOR-002** delivered. The umbrella **REQ-STOR-001**
(full stack: real driver, filesystem/object store, encryption-at-rest layer, semantic-store-on-
persistent) stays `partial` — the journal is its crash-consistent middle; the ends are follow-ons
(REQ-DRV-001 driver next). Traceability green (49 reqs — 41 delivered / 3 partial / 5 deferred).

## Delivered (2026-07-22 — REQ-BOOT-002: asymmetric component provenance, ed25519 + key hierarchy)

First P7 brick (secure-boot Phase 2, ADR-025). Phase 1's `TrustStore` was **symmetric** HMAC — the
verifier held the secret and could therefore forge. This wave adds **asymmetric** provenance in
`aletheia/src/provenance.rs` (ed25519, pure-Rust dalek; ADR-004-consistent, no C toolchain):

- **`SigningIdentity`** holds the PRIVATE key and signs a component's content hash; **`AsymTrustStore`**
  holds trusted **public keys ONLY** — so possession of the verifier's trust anchor confers **no
  ability to sign**. A compromised verifier still cannot forge (the property Phase 1 lacked).
- **Root→signing-key hierarchy:** a trusted root ENDORSES a component-signing key (`endorse`), which
  signs components; `verify_chain` accepts a component only if a trusted root endorsed its signer AND
  the signer signed the component — signing authority can be delegated/rotated without trusting every
  signer directly.
- **Fail-closed:** empty store verifies nothing; malformed keys/signatures are verification failures
  (never mistaken for valid); tamper (different hash), unendorsed signer, and untrusted-root
  endorsement are all rejected.
- **Proved:** 3 new hosted tests (`provenance.rs`, fixed-seed keypairs for determinism) + the 3
  legacy HMAC tests unchanged; aletheia suite **81 passed**; `clippy -D warnings` clean.

**Honesty (advisor):** this is a NEW requirement **REQ-BOOT-002**, marked delivered. The full
**REQ-BOOT-001 "secure boot + chain of trust"** stays `partial`: a firmware→bootloader→kernel measured
chain with a hardware root of trust (TPM/secure enclave) and anti-rollback (a monotonic counter needs
persistent secure storage) are hardware-bound (ADR-025 Phase 3) — designed, not claimed. Traceability
green (48 reqs — 40 delivered / 2 partial / 6 deferred).

## Delivered (2026-07-22 — blocking IPC + priority inheritance on ALL THREE targets; x86 exit-race fixed)

Closing the divergence the aarch64-only blocking IPC opened (GAPS2 #2, advisor's steer): REQ-IPC-010
blocking IPC and REQ-IPC-009 priority inheritance are now proved through the real user-mode path on
**all three first-class targets** — aarch64 EL0 (TTBR0), RISC-V U-mode (satp), x86-64 ring-3 (PML4) —
each **22 boundary invariants**, VM-gated:

- RISC-V: `run_blocking_ipc` + `run_priority_ipc` in `kernel-riscv64/src/usermode.rs`, new
  `_stub_recv_exit` asm stub, a0(`regs[10]`) delivery; `scripts/vm-e2e-riscv.sh` "ALL 22 … HOLD".
- x86-64: same in `kernel-x86_64/src/usermode.rs`, new `stub_recv_exit` asm stub, rdi(`regs[RDI]`)
  delivery; `kernel-x86_64/scripts/smoke-test.sh` "ALL 22 … HOLD", exit 33.
- Per-target pieces (advisor: highest-asm-content spread): a two-syscall receiver stub in each ISA,
  the `IPC_BLOCK_MODE` recv branch in each trap dispatcher, frame-register delivery per `TrapFrame`
  layout, and the priosched wiring — composed from each target's existing syscall stubs.

**Bug fixed (x86 exit-race):** the extra invariants exposed a latent race — `kmain` re-enabled
interrupts after the ring-3 suite, so a PIT IRQ latched during the suite fired between "[e2e] PASS"
and `exit(0)`; with no live scheduler left, `resume_return` jumped into the last excursion's stale
`KERNEL_CTX` → triple fault → QEMU exit 255 (the "x86 exit-255 flake"). Fixed by keeping IF=0 through
the halt/exit (as aarch64/RISC-V already do). x86 now exits 33 deterministically.

`check-traceability.sh` green (47 reqs — 39 delivered / 2 partial / 6 deferred); REQ-IPC-009/010 rows
name all three gates (GAPS2 #1). The IPC substrate (gap Issue 2) is delivered cross-target.

## Delivered (2026-07-22 — REQ-IPC-009 → delivered: priority inheritance proved end-to-end on aarch64)

The payoff of the blocking-IPC vehicle: REQ-IPC-009 priority inheritance is now proved **end-to-end
through the real blocking-IPC path** (not a hosted re-run of coherent-by-construction policy — the bar
the advisor + GAPS2 #5 set). New EL0 invariants **20-22** in `kernel/src/usermode.rs::run_priority_ipc`,
VM-gated by `scripts/vm-e2e.sh` (now **ALL 22 EL0-BOUNDARY INVARIANTS HOLD**, exit 0):

- **20 — inversion avoided at the real dispatch point:** a HIGH-priority EL0 receiver blocks on the
  endpoint a LOW-priority task services; the blocked HIGH `wait`s on that endpoint (held by LOW) so
  `kernel_core::priosched::PriorityScheduler` boosts LOW's effective priority to HIGH — and
  `schedule_next` dispatches the boosted LOW **ahead of a Ready MEDIUM** task. A priority-blind
  scheduler would have run MEDIUM and starved HIGH indirectly.
- **21 — the boosted LOW services:** the dispatched LOW runs and sends, waking the blocked HIGH.
- **22 — HIGH receives:** HIGH resumes as the highest-priority task and receives the body across the
  two distinct address spaces.

MEDIUM is a scheduler-only Ready competitor (the proof is the dispatch *decision* under real
contention, not MEDIUM's execution). `clippy -D warnings` clean; `check-traceability.sh` green:
**47 requirements — 39 delivered / 2 partial / 6 deferred**. The **entire IPC substrate scope
(gap Issue 2) is now delivered** — synchronous, transfer+attenuation, bounded queues, async
notifications, timeout, cancellation, trace/replay, zero-copy shared memory (all-target real MMU),
blocking IPC, and priority inheritance. Follow-on: spread blocking-IPC + priority inheritance to
x86-64/RISC-V (the aarch64 proof is the reference); GAPS2 #5 closed on the dev backend.

## Delivered (2026-07-22 — REQ-IPC-010: real blocking IPC on aarch64, the vehicle for priority inheritance)

The chosen next feature (advisor: "the cheap-conversion phase is over; pick a feature"). Until now the
kernel IPC endpoint was a non-blocking single-slot mailbox (`recv` on empty returned a fail-value).
This wave adds **real blocking IPC** on the aarch64 dev backend — the substrate REQ-IPC-009 priority
inheritance needs to be VM-proved end-to-end. New EL0 invariants **17-19** in
`kernel/src/usermode.rs::run_blocking_ipc`, proved live by `scripts/vm-e2e.sh` (now **ALL 19
EL0-BOUNDARY INVARIANTS HOLD**, exit 0):

- **17 — recv blocks:** a receiver that `recv`s an EMPTY endpoint is genuinely descheduled — the
  handler signals the block, and the scheduler moves it to `Blocked` (`kernel_core::sched` block()).
- **18 — send wakes + delivers:** the sender's `send` deposits the body; because a receiver is
  blocked-waiting, the kernel wakes it (`unblock` ⇒ `Ready`) and delivers the body across the two
  distinct TTBR0 address spaces (into the receiver's saved `x0`).
- **19 — receiver resumes:** the woken receiver resumes *past its `svc`* with the body in `x0` and
  exits reporting it — proving the block→wake→deliver→resume round trip, not just a mailbox drain.

Guarded so every other test is untouched: blocking is behind an `IPC_BLOCK_MODE` flag (default off),
so `run_ipc`'s non-blocking mailbox semantics are unchanged. `clippy -D warnings` clean;
`check-traceability.sh` green. **This is the vehicle, not the priority proof:** REQ-IPC-009 stays
`partial` until a follow-on drives this blocking endpoint under `PriorityScheduler` with a 3rd
medium-priority task, showing the boosted holder runs ahead of it (priority inversion avoided) — the
genuine end-to-end priority-inheritance VM proof (GAPS2 #5).

## Delivered (2026-07-22 — REQ-IPC-008 → delivered: grant-table through the REAL aarch64 MMU path)

Converting the zero-copy shared-memory grant-table from hosted-only (`partial`) to VM-gated
(`delivered`) by driving it through the real per-target path — the honesty currency this project runs
on (GAPS2 #3: "the real path is what matters"), the same move that made REQ-KERN-005 real. The shared
`GrantTable` is the arch-independent authority/lifecycle layer; the aarch64 `vm.rs` performs the actual
mapping (the documented seam). New EL0 invariants **14-16** in `kernel/src/usermode.rs::run_shared_memory`,
proved live by `scripts/vm-e2e.sh` (now **ALL 16 EL0-BOUNDARY INVARIANTS HOLD**, exit 0):

- **14 — capability-gated (fail-closed):** without a `memory.share` capability the grant is refused and
  nothing is mapped; with it, the share is authorized through the SAME `CapEngine` the pipeline uses.
- **15 — zero-copy across address spaces:** the grant maps ONE physical frame into TWO distinct
  process TTBR0 roots, and both resolve the shared VA to the SAME physical frame — one page present in
  two separate address spaces is exactly zero-copy shared memory (no copy through any queue).
- **16 — revocation unmaps:** revoking the grant tears down the grantee's page mapping while the
  grantor keeps its own access (the per-target "revoke ⇒ unmap" seam).

`check-traceability.sh` green; `clippy -D warnings` clean on the aarch64 kernel.

**All three targets now prove it** (cross-target, GAPS2 #2): the identical `run_shared_memory` invariants
14-16 pass on aarch64 (TTBR0, `scripts/vm-e2e.sh`), RISC-V (Sv39 satp, `scripts/vm-e2e-riscv.sh`, "ALL
16 USER-MODE BOUNDARY INVARIANTS HOLD"), and x86-64 (PML4, `kernel-x86_64/scripts/smoke-test.sh`, "ALL
16 RING-3 BOUNDARY INVARIANTS HOLD", exit 33) — one arch-independent `GrantTable` authority layer, three
real per-target MMU backends. **Follow-on (GAPS2 #3):** wiring EL0/ring-3/U-mode code itself (not just
the kernel-verified mapping) to read/write the shared page across the boundary.

## Delivered (2026-07-22 — GAPS2 Issue #1: target-specific traceability, no gate can be escaped)

A second architecture audit (`docs/ARCHITECTURE-GAPS2.md`) flagged that the traceability matrix proved
*evidence files exist* but not that the *correct target* executes them: the generic `REQ-USER-*` /
`REQ-MEM-*` rows listed several implementations yet omitted `kernel-x86_64/scripts/smoke-test.sh` from
their VM-Gate column, so an x86-64-specific user-mode or memory regression could compile and **escape
the requirement gate**. Fixed by splitting those into **one row per target**, each naming that target's
own implementation and its own VM gate:

- `REQ-USER-AARCH64-001` / `REQ-USER-X86-001` / `REQ-USER-RISCV-001` (EL0 / ring-3 / U-mode, each →
  `vm-e2e.sh` / `smoke-test.sh` / `vm-e2e-riscv.sh`).
- `REQ-MEM-AARCH64-001` / `REQ-MEM-X86-001` / `REQ-MEM-RISCV-001` (frame allocator + MMU per target →
  the same three gates). (Kernel-boot rows `REQ-KERN-001/002/003` were already per-target.)

A regression in one target's user-mode now fails **that target's** named gate, not a sibling's.
`check-traceability.sh` green: **46 requirements — 36 delivered / 4 partial / 6 deferred** (the
authoritative counts; dated sections below are point-in-time snapshots). Remaining GAPS2 items tracked:
cross-target conformance suite (#2), IPC-transfer through the real per-target user-mode path (#3),
SMP + capability concurrency spec (#4/#9), end-to-end VM-tested priority inheritance (#5), real
secure-boot chain (#6), persistent storage (#7), fault supervision (#8).

## Delivered (2026-07-22 — REQ-KERN-005: aarch64 target DRIVES the shared kernel-core scheduler, VM-gated)

The "wire, don't pile" brick: instead of adding a fourth unwired kernel-core policy module, this wave
makes a real target *drive* the shared scheduler, converting REQ-KERN-005 from `partial` (policy +
hosted tests, nothing drove it) to `delivered` (a target uses it, VM-gated) — the honest delivered bar.

- The aarch64 dev backend's cooperative multitasking (`kernel/src/usermode.rs::run_scheduler`) no
  longer hand-rolls its `(cur+k)%NTASK` rotation. It now drives `kernel_core::sched::RoundRobin`:
  `schedule_next` decides which EL0 task runs next, a yielded task is rotated to the tail, an exited
  task is `finish`ed and leaves the rotation. The target performs ONLY the context-switch *mechanism*
  (`resume_frame` + TTBR0 address-space switch) behind the `TaskContext` seam.
- Reproduces the exact `A,B,A,B,A,B,A,B` sequence the bespoke loop did — **VM-proved live**, not just
  argued: `scripts/vm-e2e.sh` re-passes EL0 invariants 6 (round-robin to completion), 7 (each task
  resumes with its own register-magic at the shared VA), 8 (distinct TTBR0 spaces), and 9 (timer
  preemption round-robins) with the shared scheduler in the loop; exit 0, all 11+7+13+13 invariants.
- **ALL THREE first-class targets now drive the one shared scheduler** — RISC-V
  (`kernel-riscv64/src/usermode.rs`, VM-gated by `scripts/vm-e2e-riscv.sh`, U-mode invariants 6/8) and
  x86-64 (`kernel-x86_64/src/usermode.rs`, VM-gated by `kernel-x86_64/scripts/smoke-test.sh`, ring-3
  invariant 6, exit 33) drive the identical `kernel_core::sched::RoundRobin` via the same swap. Each
  target performs ONLY its arch context switch behind the `TaskContext` seam — the "one coherent
  kernel, three backends" thesis (gap Issue 1 / GAPS2 Issue 2) is now real for the scheduler policy.
- **Follow-on (documented):** driving the `PriorityScheduler`/`GrantTable` from a target (the path to
  REQ-IPC-008/009 `delivered`, GAPS2 Issues 3/5), and a formal cross-target conformance suite
  (GAPS2 Issue 2). (Pre-existing, non-CI-gated clippy lints in the x86-64 kernel — `gdt.rs` descriptor
  dead-code, `run_in_space` arg count — are untouched by this brick; a separate cleanup.)

## Delivered — kernel-core policy, PARTIAL (2026-07-22 — REQ-IPC-009: priority inheritance + priority-aware scheduling)

**Status honesty (traceability `partial`):** this is the arch-independent *policy* + hosted proof; it
is the same shape as REQ-KERN-005 — no target drives it and no VM gate exercises it yet, so it is
`partial`, not `delivered`, until wired into a target's `usermode.rs` and VM-gated (the accumulated
kernel-core policy — KERN-005 scheduler, IPC-008 grant-table, IPC-009 priority scheduler — is wired in
one target-integration brick next).

The second remaining IPC scope item, closing the IPC substrate's kernel-core policy work (gap Issue 2
/ ADR-020). The round-robin scheduler is fair but priority-blind, so it is prey to **unbounded
priority inversion**: a high task H blocks on an endpoint a low task L holds while an unrelated medium
task M preempts L forever. `kernel-core/src/priosched.rs` (`PriorityScheduler`) breaks this the way a
real microkernel does, as arch-independent policy inherited by all three targets:

- **Priority donation** — when a task `wait`s on an endpoint held by another, the holder's *effective*
  priority rises to the waiter's, and **transitively** to anything blocked behind it across a chain of
  held endpoints. Effective priority is derived on read (a visited set makes a deadlock cycle
  terminate rather than hang), so `release` withdraws donation automatically.
- **Inversion avoided** — `schedule_next` runs the highest-effective-priority Ready task (FIFO
  tiebreak), so a boosted low holder outranks an unrelated medium task and finishes its critical
  section; the inversion is bounded to that section.
- **Capability-gated** — acquiring or waiting on a kernel endpoint is authorized by the SAME
  `CapEngine` (no ambient endpoint access); every refusal is fail-closed.
- **Hand-off on release** — releasing an endpoint hands it to its highest-priority waiter, which is
  unblocked and becomes the new holder.
- **Per-target seam (ADR-010):** the scheduling *policy* is here; the actual register save/restore +
  address-space switch stays each target's `TaskContext` seam (same split as `kernel-core::sched`).
- **Proved on the host** — `kernel-core/tests/priosched.rs` (9 tests): base-with-no-donors, cap-gated
  fail-closed, acquire/busy/wait semantics, single + transitive inheritance, inversion avoidance,
  release-withdraws-and-hands-off, and non-holder-release fail-closed. `kernel-core` hosted suite now
  **58 passed** (8 suites); `clippy -D warnings` clean; `check-traceability.sh` green (34 delivered / 5
  partial / 6 deferred — IPC-008/009 are `partial`: kernel-core policy proved, per-target wiring +
  VM gate pending). The IPC scope's remaining items are the per-target wiring of this policy and
  wiring `send_transfer` into each target's cross-address-space `usermode.rs` fast-path.

## Delivered — kernel-core policy, PARTIAL (2026-07-22 — REQ-IPC-008: zero-copy shared-memory grant-table)

**Status honesty (traceability `partial`):** arch-independent *policy* + hosted proof, not yet driven
by any target or VM gate — `partial` like REQ-KERN-005 until wired into a target's `vm.rs` mapping and
VM-gated.

The bulk-data companion to the message-copy `Channel`, closing the first of the two remaining IPC
scope items (gap Issue 2 / ADR-020). The synchronous fast-path copies a message body into the
receiver's inbox — correct for control messages, wrong for a page of data. A real microkernel shares
one physical frame region between endpoints under explicit authority; this wave delivers the
**arch-independent authority + lifecycle layer** of that mechanism in `kernel-core/src/grant.rs`
(`GrantTable`), inherited by all three targets from one source.

- **Capability-gated establishment** — a share requires `memory.share` authority checked through the
  SAME `CapEngine` the pipeline uses; no capability ⇒ no grant (fail-closed).
- **Attenuation, never amplification** — a grant can only narrow the grantor's own access; a read-only
  holder can never mint a read-write grant (the memory analogue of `CapEngine::delegate`).
- **Zero-copy backing** — the region's bytes live exactly once (`Rc<RefCell<[u8]>>`); a read-write
  holder's write is observed by every reader with no copy through any queue, made observable by
  `region_refcount` (rises per live grant, falls on revoke).
- **Bounded access** — every read/write is confined to `[0, len)` (the model of the MMU refusing an
  access past the shared frame); an `offset+len` overflow is refused, never wraps.
- **Revocation unmaps** — revoking a grant drops that endpoint's handle fail-closed (later access
  denied) and releases its share of the backing.
- **Per-target seam (ADR-010):** turning a granted region into a real page-table mapping in each
  endpoint's address space is each target's `vm.rs` (map/unmap already delivered) — the same split by
  which `kernel-core::sched` owns the scheduling policy while each target owns the context switch.
- **Proved on the host** — `kernel-core/tests/grant.rs` (8 tests): cap-gated, zero-copy read-sees-write,
  read-only-cannot-write, attenuate-but-never-amplify, no-share-without-access, bounded, and
  revocation-unmaps-and-releases. `kernel-core` hosted suite **49 passed** at this brick (7 suites);
  `clippy -D warnings` clean. Traceability status `partial` (kernel-core policy proved; per-target
  `vm.rs` mapping + VM gate pending).

## Run it

```bash
cd aletheia
cargo test        # 81 passed (13 suites) — M1 acceptance + conformance + property + security + P2 component + policy + AI
cargo run -- serve  # long-running Core Alpha behind the Unix-socket IPC boundary (clients issue Requests)
cargo test --test component   # the 14 P2 WASM-component acceptance + fuzz tests
cargo run         # aletheiad: boots the hosted System Core + runs the UC-001..004 demo with traces

(cd ../kernel-core && cargo test)  # 322 passed (28 suites) — the shared spine invariants + IPC substrate (async/timeout/cancel/trace-replay) + adversarial security-behaviour suite + arch-independent scheduler, proved on the HOST, no QEMU
./scripts/check-traceability.sh    # requirement traceability gate: every delivered/partial requirement maps to existing impl+test evidence (gap Issue 12)

./scripts/e2e-all.sh         # ONE command, all three targets: aarch64 + RISC-V QEMU gates + x86-64 disk-image smoke-test -> single PASS/FAIL
./scripts/vm-e2e.sh          # aarch64 microkernel in QEMU: 13 spine + 21 memory + 66 virtual-memory + 24 EL0 user-mode + 20 risk-advisor + 21 virtio-blk + 22 SMP invariants + exit 0
./scripts/vm-e2e-riscv.sh    # RISC-V/RV64GC first-class target (QEMU virt + OpenSBI, S-mode): 13 spine + 21 memory + Sv39 vm + U-mode + 20 risk-advisor + 22 SMP invariants + exit 0
./scripts/linux_pipe_bench.sh # real-Linux IPC baseline for the perf discussion (needs Docker)
```

### Boot the OS end-to-end

```bash
# The NEW P5 memory-management work (frame allocator + MMU) runs on the aarch64 dev backend.
# Boot it directly as a -kernel ELF in QEMU (this IS the e2e VM test):
cd kernel && cargo run          # boots Aletheia, proves 11+21+49+22 invariants live (incl. EL0 user-mode + preemptive multitasking), exits 0

# A real bootable DISK IMAGE (Aletheia as its own OS on AMD64/x86-64 under UEFI):
cd kernel-x86_64 && bash scripts/build-image.sh   # macOS host -> build/aletheia-x86_64.{img,vmdk}
#   ...or portable (Linux/CI, mtools only — no hdiutil/root): bash scripts/build-image-linux.sh
bash scripts/smoke-test.sh                         # boot the image in QEMU+OVMF, assert exit 33
#   • QEMU:       qemu-system-x86_64 -bios <OVMF_CODE.fd> -drive format=raw,file=build/aletheia-x86_64.img -serial stdio
#   • VMware:     attach build/aletheia-x86_64.vmdk to a UEFI VM
#   • VirtualBox: attach build/aletheia-x86_64.img (see scripts/build-vbox.sh)
# NOTE: the x86-64 image now proves 22 memory (frame allocator from the UEFI map + ownership + erase-on-free)
# + 40 virtual-memory (map/unmap + reclamation + teardown + W^X audit over the live UEFI PML4 hierarchy) + 11 spine + 22 SMP (MADT +
# INIT-SIPI-SIPI at -smp 4) + 22 ring-3 invariants. x86-64 can't do aarch64's "MMU off->on" flip (long mode requires
# paging), so its vm suite proves the honest subset: walk + edit the live hierarchy.
# smoke-test.sh boots -smp 4 and gates all four marker families + exit 33.
```
