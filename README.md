# Aletheia

**A from-scratch, AI-native operating system.** Not Linux, not a Linux distribution, not a Linux
derivative — its own kernel, process/memory model, IPC, capability architecture, runtime, storage,
and service model, Rust-first. The current macOS-hosted Rust implementation is a **temporary
development environment**; every interface is an Aletheia-owned abstraction designed to be
re-implemented natively later without rewriting the semantic or security architecture.

## The idea

Intelligence is a **native but untrusted** collaborator. A model may interpret language, reason over
supplied context, and *propose* structured plans — but it never holds authority and never executes
anything. Aletheia independently validates, authorizes, approves, executes, and verifies every
operation. Authority is always an explicit, scoped, revocable **capability** — there is no ambient
authority anywhere in the system.

The whole OS turns on one deterministic pipeline:

```text
Intent → build context (World Model + capabilities + memory)
       → ModelProvider proposes a structured Plan
       → schema + semantic validation
       → capability evaluation (authority)
       → policy / human approval (governance)   ← independent of authority
       → execution
       → independent verification
       → immutable provenance event
```

The AI is the only probabilistic stage. Everything after it is deterministic and testable.

## Layered architecture

```text
EXPERIENCE / APPLICATIONS        explainable traces, world view, audit — CLIENTS of the boundary
        │  Service API / IPC boundary (service.rs): Request/Response over in-proc + Unix socket
SYSTEM CORE (aletheia/src)
  ├── domain            seven primitives: Entity·Capability·Context·Intent·Action·Memory·Relationship
  ├── storage           content-addressed, versioned, encrypted-at-rest, durable semantic store
  ├── capabilities      the sole AUTHORITY engine: mint · attenuated delegation · revocation · evaluate
  ├── policy            the GOVERNANCE engine: human approval, separate from capability authority
  ├── worldmodel        typed, provenance-bearing relationship graph + traversal
  ├── intent_action     the deterministic pipeline (parse · validate · authorize · execute · verify · record)
  ├── ai/               the AI subsystem (below) — model-agnostic
  ├── component         capability-secure WASM sandbox (no ambient authority), explicitly versioned ABI, bounded in EVERY dimension — fuel, memory, tables, stack, wall clock (ADR-065/066)
  ├── agents            first-class, capability-bounded, revocable actors
  ├── syscore           composition root wiring the pipeline + task lifecycle + approvals
  └── service           capability-gated Service API + IPC (in-process + Unix socket) — the app boundary
MICROKERNEL                      no_std Rust microkernel, boots on QEMU, re-proves the invariants (P4)
   kernel/ · kernel-x86_64/ ·        aarch64 (-kernel) · AMD64/x86-64 (UEFI/OVMF) · RISC-V (SBI, S-mode)
   kernel-riscv64/                   — same shared spine.rs/selftest.rs, three CPU targets, all VM-tested
        │  Aletheia HAL — arch-independent contract per crate; no Linux/macOS/POSIX imports
HARDWARE                         AMD64/x86-64 · RISC-V (first-class targets) — aarch64 (bootstrap/dev)
```

Dependency direction points inward toward `domain`; nothing reaches around the capability engine.
Aletheia is its own OS: AMD64 and RISC-V are hardware targets, Rust is the implementation language,
and every OS abstraction belongs to Aletheia (ADR-019).

### Authority vs. governance (two independent axes)

- **Capabilities** answer *authority*: is this subject permitted to do this at all? → `Allow / Deny /
  RequireApproval`. Unforgeable, possession-based, attenuated on delegation, cascading revocation,
  fail-closed.
- **Policy** answers *governance*: even when authorized, must a human approve this? A destructive op
  with full authority still needs approval; an approval-constrained capability needs approval even
  for a safe read. Pending approvals are durable (replayed from the event log) and bound to the exact
  intent — approval confers no authority.

## The AI subsystem (`aletheia/src/ai/`)

AI is a **first-class, Aletheia-owned subsystem** behind a model-agnostic `ModelProvider`. The model
runtime is an implementation detail: the Core never depends on llama.cpp APIs or a hardcoded model.

```text
ai/
├── provider   ModelProvider trait (one seam — a native Aletheia model service implements it later)
├── config     AiConfig: registry default → persisted selection → env (MODEL_REF / MODEL_PATH / …)
├── registry   the pinned model set + the operator's selection (ADR-052)
├── context    native Context Engine (Context Fabric) — capability-aware, structured-first, budgeted
├── prompt      intent/planner protocol + plan grammar / JSON schema + <think>-stripping extraction
├── runtime    HF-cache GGUF discovery + llama-server lifecycle
├── bench      the operation-surface benchmark + the served-model identity check (ADR-052)
└── llama       LlamaCppProvider — dependency-free localhost HTTP to llama-server; deterministic fallback
```

### Which model, and how you change it

The model is a property of the **system**, not a constant in the source — and the catalog is
**discovered, not declared**. Aletheia scans the local model cache (honoring `HF_HUB_CACHE` /
`HF_HOME`) and lists what is really on the machine:

```console
$ aletheiad model list
   id                         quant           size  state
   bge-small-en-v1.5          -             64 MiB  present, unpinned
*  lfm2.5                     Q4_K_M      1596 MiB  present, pinned, default
   minicpm                    Q8_0        1100 MiB  present, pinned
   qwen3.6-40b-gr…o-max-mtp   -          22583 MiB  present, unpinned
   aletheia-lm                -                  -  not yet trained
```

Nothing in the source names those first and fourth rows; they are simply there. **Manifests
(`models/*.toml`) characterize models, they do not enumerate them** — a manifest carries only what a
directory listing cannot: the checksum a file should have, sampling parameters that were *measured*,
whether the chat template forces a `<think>` phase, and which structured-output strategy actually
works. A model with no manifest is still listed and still runnable, marked **`unpinned`** so it is
clear its parameters are defaults rather than findings.

```bash
aletheiad model list          # what this machine actually has; * marks the running selection
aletheiad model use lfm2.5    # switch — a unique prefix is enough; persisted under $HOME/.aletheia
aletheiad model status        # selected model, weights, checksum verification, what is being served
aletheiad model pull          # fetch the selected model's weights (never committed to the repo)
aletheiad model bench         # run the whole operation surface through it (below)
```

`aletheia-lm` — **this OS's own model** — is characterized before its weights exist, so the switch
can be lined up now. Selecting it says `NOT YET TRAINED` and names the environment variable to point
at the finished weights; a file at that path flips it to runnable with no edit to any source. It does
**not** quietly fall back to another model. When no model is available at all, the **deterministic
interpreter** takes over — the OS is fully functional with no resident model, and that interpreter is
also the test oracle.

Model quirks live in the manifest, not in the provider: MiniCPM is a *thinking* model whose forced
`<think>` phase collides with a strict grammar (so it runs in no-think mode), while LFM2.5 returns
**empty output** under that same grammar and is constrained by JSON schema instead. One model's
workaround is not every model's — which is also why an *uncharacterized* model gets the schema path
rather than the grammar.

`model status` verifies the pinned checksum by streaming the file, and reports `verified`,
`MISMATCH`, `not pinned` or `unreadable` as four distinct outcomes — "I did not check" and "it does
not match" are different facts. It runs there rather than in front of every interpretation, because
hashing gigabytes takes seconds and a check that makes the OS slow is a check somebody turns off.

### Does the model actually drive the OS?

`aletheiad model bench` asks that question rather than assuming it. One intent per registered
operation goes through the **same** provider and the **same** validation the pipeline uses; the
deterministic interpreter runs the identical set as a control arm. Before any number is recorded, the
backend is asked what it is serving and the answer must match the selected manifest — the endpoint is
a *port*, and a benchmark that measures whatever holds it would publish another model's latency under
this model's name.

Measured on one workstation (LFM2.5-2.6B-Q4_K_M, llama.cpp, `-c 8192`): **6/6 operations planned
correctly on two consecutive runs**, median ~3.5 s, control arm 6/6 at 0 ms. Getting there closed
four real defects — including a health probe that had tried only the first resolved address since
ADR-017, which made a *running* model indistinguishable from no model at all. See
[ADR-052](docs/adr/ADR-052-the-model-is-a-system-property.md).

This benchmark covers the hosted Core's **six operations**. It does **not** drive the kernel console
(`kernel-core/src/shell.rs`), which runs in kernel space with no inference engine underneath it and
has its own gate, `scripts/console-e2e.sh`.

## The machine learning that runs *inside* the kernel

Two different things in this repository are called a model, and conflating them is the easiest way to
be wrong about what Aletheia does.

The **language** model above interprets intent and proposes plans, in user space, holding no
authority. The second one is a **frozen gradient-boosted decision forest compiled to integer
comparisons**, it lives in the microkernel, and it is consulted by the scheduler for the whole life
of the machine. It answers one tabular question a few million times an hour — *is this task going to
die if I admit it?* — which is the wrong question to ask a language model at any price: four orders of
magnitude too slow, needing floating point this kernel does not have, and not reproducible run to run.

* Trainer, corpus, calibration and exporter: [`aletheia-ml`](../aletheia-ml) (Google Borg 2019 cluster
  trace, 32.7 M held-out rows).
* In the kernel: `kernel-core/src/mlrisk.rs` (verify + evaluate), `taskfeat.rs` (derive the features
  from a live task), `mlsched.rs` (residency, counters, the live admission path).
* Shipped blob: `kernel-core/models/aletheia_risk.altm`, embedded with `include_bytes!` — 171 trees,
  26 469 nodes, worst case **1 368 integer compares per advice**, no allocation after load, no
  floating point anywhere.

### Proof 1 — the model is built into the OS, and verified by it

The blob is part of the image, so a running kernel cannot be holding a model the image hash does not
account for. Embedding is not trusting: **every target verifies it at boot** and prints what it got,
or the named `ModelError` that refused it. Wrong magic, wrong version, wrong feature count, a feature
contract that does not match the one this kernel was compiled against, a child index out of range, a
truncated tail — each is a refusal by name, and a model the kernel cannot verify is a model the kernel
does not run.

From a real QEMU boot (`scripts/vm-e2e.sh`, aarch64; the RISC-V gate prints the same):

```text
[mlrisk] bundled forest: 171 trees, 26469 nodes, worst case 1368 compares per advice
[mlrisk] ALL 22 RISK-ADVISOR INVARIANTS HOLD
[mlrisk-stress] ALL 8 STRESS INVARIANTS HOLD
[mlsched] RESIDENT: 171 trees, 26469 nodes, worst case 1368 compares per advice
[mlsched] ALL 12 LIVE-ADVISORY INVARIANTS HOLD
```

Twenty of those invariants check the model against the trainer — every fixed-point margin and every
three-way verdict reproduced **exactly** on the committed parity fixture, in kernel space, on the CPU.

### Proof 2 — it is resident and consulted while the machine runs, not just at boot

A blob loaded inside a boot selftest and dropped on the way to the shell is an *installed* model, not
a running one. `kernel-core/src/mlsched.rs` holds one verified forest behind one lock for the whole
uptime; every admission on the priority-scheduler path goes through it, every dispatch and every task
death is fed back into the history the *next* advice reads, and the cell-pressure census ages on every
console line even when nothing is being admitted.

**And a real user-mode task reaches it.** On all three targets, two genuine ring-3 / U-mode tasks —
own address spaces, own trap frames, real context switches — are admitted through the resident
advisor and dispatched by `PriorityScheduler`, with each dispatch and each exit fed back. Each task
is described with the memory it actually mapped, not a plausible-looking constant. Three boot
invariants per target gate it, including `the advisor was consulted once per real user-mode task — a
live spawn reaches the model`. The interleaving is deliberately *not* asserted: advice may reorder
equals, and demanding a fixed order would be asserting the advisor had no effect. That every task
gets every slice and exits is what is asserted.

x86-64 was wired last, deliberately: its ring-3 gate was red on a defect predating this work, and
wiring a model into a target whose user-mode gate cannot pass proves nothing about either. Fixing it
found something worth stating — `SYS_REGCHECK` returned a conventional `0`, that return value lands in
the task's `rax`, and `rax`'s sentinel *is* `SYS_REGCHECK`. The stub's second `int 0x80` therefore
dispatched syscall 0, and the **restore** half of that invariant had never once been exercised on
x86-64. The invariant was not flaky; it was measuring nothing.

The boot commissions it against live state and reports what it did:

```text
[mlsched] commissioning: 4096 tasks admitted over 28665 s of machine time (95 cell bins)
[mlsched] live census: 4096 advices — 180 low / 3543 elevated / 373 abstain (0 in band), 373 out-of-box
[mlsched] watching: 4096 dispatches, 2457 finished / 820 failed / 819 evicted, 4096 ticks
[mlsched] continuity: span 28665 s, longest gap between advices 7 s
[mlsched] advised drain is a permutation of the model-free one: 4096 tasks in, 4096 tasks out
```

And residency is a question you can ask the running machine yourself, at any later moment in a
session, rather than a claim this file makes on its behalf — `mlstat` at the console reads the live
counters:

```text
risk advisor: RESIDENT — 171 trees, 26469 nodes, worst case 1368 compares per advice
advices: 4096 (180 low / 3543 elevated / 373 abstain, 0 of those in the conformal band)
decisive: 90.8% — 373 out-of-box arrival(s) declined
watching: 4096 dispatch(es), 2457 finished / 820 failed / 819 evicted, 4096 housekeeping tick(s)
continuity: first advice at 0s, last at 28665s (span 28665s), longest gap 7s
silence: 0s since the last advice, as of the machine's clock at 28665s
```

Those six lines are `mlstat`'s own renderer (`shell::report_risk_advisor`) — one implementation, so
the boot banner and the console command cannot say different things — and the boot prints them by
calling it. The console command is separately gated: `console: mlstat reports the resident risk
advisor's live counters` is one of the 42 console invariants every target re-proves in QEMU.

`silence` is the line that makes "continuously active" **falsifiable**. A historical gap only ever
closes when the *next* consultation arrives, so an advisor that fell silent an hour ago would still be
reporting the small gaps it managed while it was busy; silence is measured against the machine's own
clock at its most recent tick and grows with the machine. `mlstat` prints both.

### Proof 3 — consulting it makes the scheduler measurably better

Everything above proves the model *runs*. Whether it *helps* is a different question, and PR-AUC does
not answer it — a scheduler does not run a classifier, it runs a queue. So `python -m aletheia_ml
schedsim` replays held-out tasks through **the kernel's own selection rule**, twice, and changes
exactly one thing between the arms.

The comparison is deliberately unflattering to the model:

* Both arms run `PriorityScheduler::schedule_next`: highest base priority first, FIFO among equals.
  The advised arm differs in one respect only — among tasks of **equal** priority, a decisive `low`
  is preferred over a decisive `elevated`. Priority is never traded for risk, an abstention moves
  nobody, no task is dropped, delayed past its band, or denied admission.
* Verdicts come from the exported **integer** forest — the same margins, threshold and conformal band
  the kernel compares against — not from XGBoost's float path.
* The stream is the untouched chronological **test** split; nothing was fitted, calibrated or
  thresholded on these rows.
* The labels are the trace's own terminal events. The model does not mark its own homework.

Metric: **head-of-line delay for tasks that survive** — the mean number of dispatch steps a task that
would have completed spends waiting behind everything else, over a bounded 64-deep ready queue. Three
independent 60 000-task windows of the held-out split:

| window | base positive rate | `low` verdicts | of which survived | model-free wait | advised wait | change |
|---|---|---|---|---|---|---|
| seed 11 | 0.8149 | 1 478 | 98.71 % | 49.706 | 48.994 | **−1.43 %** |
| seed 18 | 0.7477 | 194 | 94.85 % | 27.873 | 25.871 | **−7.19 %** |
| seed 25 | 0.7828 | 1 009 | 63.23 % | 47.080 | 45.508 | **−3.34 %** |

**3 of 3 windows improved; mean −3.99 % (min −1.43 %, max −7.19 %).** Dispatch counts were identical
in every arm, and the doomed tasks' wait rose by 0.2–0.7 steps — the trade is visible rather than
hidden. Artifact: `aletheia-ml/artifacts/borg2019/schedsim_kernel.json`.

For completeness, the classifier numbers on the same untouched split (32 733 226 rows, 79.02 %
positive): PR-AUC **0.9954** against a 0.7902 base rate (lift 1.26x), ROC-AUC **0.9827**, integer-path
vs float-path decision agreement **1.000**.

### Proof 4 — against the selection rule every major OS family actually ships

"Better than *not* consulting the model" invites the obvious next question: better compared to a real
operating system? `python -m aletheia_ml oscompare` answers it under terms set out before any number
is produced.

**Nothing here boots any of these systems, and nothing here measures one.** Booting them would not
answer the question anyway — comparing scheduling *policy* means feeding the same arrivals, with the
same service demands, to each policy, and no two kernels are ever handed the same workload on the same
hardware at the same instant. So each system's **documented selection rule** — the function answering
"which runnable thread runs next" — is implemented in one simulator and driven from one trace.

Metric: **mean turnaround of tasks that survive** (dispatch steps from arrival to last slice), over
three windows × 20 000 held-out Borg tasks with a 64-deep ready queue. Work that completes is the work
a machine exists to do; the doomed tasks' turnaround is printed beside it so the trade stays visible.

| policy | survivor | doomed | advised better by | what it is |
|---|---|---|---|---|
| `xnu-macos` | 81.175 | 69.513 | **25.20 %** | Darwin/XNU (macOS, iOS): base priority minus decayed CPU usage |
| `redox-rr` | 73.899 | 73.012 | **17.83 %** | Redox: round-robin over runnable contexts |
| `fifo` | 73.258 | 73.194 | **17.12 %** | arrival order, no policy at all |
| `freebsd-ule` | 72.229 | 73.840 | **15.93 %** | FreeBSD ULE: interactivity + nice, current/next queue swap |
| `linux-cfs` | 68.477 | 75.042 | **11.33 %** | Linux ≤ 6.5 (Debian 12, RHEL 8/9, Ubuntu ≤ 24.04): smallest virtual runtime |
| `linux-bore` | 68.156 | 75.031 | **10.91 %** | CachyOS / Zen / Liquorix BORE: EEVDF plus a burst penalty |
| `zircon-fair` | 68.055 | 75.202 | **10.78 %** | Fuchsia Zircon: weight-proportional virtual finish time |
| `linux-eevdf` | 68.055 | 75.202 | **10.78 %** | Linux 6.6+ (Fedora, Arch, Ubuntu 24.10+, Debian 13): earliest eligible virtual deadline |
| `linux-muqss` | 64.560 | 76.269 | **5.95 %** | MuQSS / BFS (-ck, Liquorix): earliest virtual deadline, no fairness accounting |
| `windows-nt` | 64.295 | 76.702 | **5.56 %** | Windows NT/10/11: 32 levels, RR within level, anti-starvation boost |
| `sel4-prio-rr` | 61.643 | 77.517 | **1.50 %** | seL4: strict priority, round-robin within a priority, no aging |
| `linux-rt-fifo` | 61.643 | 77.517 | **1.50 %** | PREEMPT_RT `SCHED_FIFO` (RHEL for Real Time, audio distros) |
| `aletheia-free` | 61.643 | 77.517 | **1.50 %** | Aletheia with no model resident |
| `solaris-ts` | 61.621 | 77.522 | **1.46 %** | Solaris / illumos timeshare: `ts_dptbl`, quantum expiry drops priority |
| `aletheia-advised` | **60.720** | 77.883 | — | Aletheia, forest resident: the same rule, plus low-over-elevated **among equals** |

Total dispatched work was identical in all fifteen arms, so no policy "won" by doing less.

**Now read that table the way this repository requires, because it does not say what a marketing
version of it would.**

* **Only 1.50 points of the whole spread come from the machine learning.** That is the
  `aletheia-advised` vs `aletheia-free` gap, and it is the only column the model is responsible for.
  Everything else is *strict priority scheduling*, which is not an Aletheia invention and which any
  kernel can adopt — as `sel4-prio-rr` and `linux-rt-fifo` do, landing on the identical number.
* **The gain is bought, not free.** Doomed tasks wait longer in every priority-aware arm (77.9 steps
  against Redox's 73.0). The fair schedulers are not losing because they are worse; they are doing
  precisely what they were designed to do — refuse to starve anybody — and this workload's surviving
  tasks happen to correlate with Borg priority.
* **Three arms are handicapped by the workload, and it is not their fault.** XNU's usage decay, ULE's
  interactivity score and Solaris' long-wait boost are all driven by threads *sleeping*. Borg tasks in
  this trace are CPU-bound and never sleep, so those designs degenerate here towards usage-penalised
  round-robin. `xnu-macos` finishing last is a statement about this workload, not about macOS.
* **`zircon-fair` and `linux-eevdf` are identical because their published selection rules are.**
  Fuchsia's fair scheduler picks by weight-proportional virtual finish time; that is EEVDF's rule. No
  attempt was made to invent a difference.
* **This is a comparison of pick functions, not of operating systems.** No preemption latency, cache
  or NUMA effects, wakeup placement, load balancing, cgroups or energy model. A modern scheduler is
  far more than its pick function, and the omitted parts are where the decades went.

The defensible claim is the narrow one the model was built for: **none of the other fourteen rules
know which arrivals are going to die.** Every kernel in that table schedules a task that will be
evicted in ninety seconds exactly like one that will run to completion, because nothing in the kernel
can tell them apart. Aletheia's forest can, on data it has never seen — and 1.50 % is what that
knowledge is worth to this queue under this metric. Small, real, and measured rather than asserted.

Artifact: `aletheia-ml/artifacts/borg2019/oscompare_kernel.json`. Reproduce with
`python -m aletheia_ml oscompare --rows 20000 --repeats 3`.

### What it is not allowed to do

The forest is **advisory by construction** (INV-014), and the invariants are written to make that
falsifiable rather than aspirational:

* it returns an ordering *hint* — never a plan, an action, a capability, or an admission verdict;
* every invariant and capability check holds identically whether it is loaded, absent, or wrong;
* with no model resident, scheduling is **bit-identical** to the model-free kernel — asserted, not
  assumed, on the host and on every boot;
* it **abstains** rather than guesses, inside the conformal band or outside the feature box it was
  fitted in;
* absence is *named*: a refused blob prints which check refused it, and the machine keeps running
  model-free rather than quietly behaving as though it were advised.

**Known and named:** the shipped borg2019 blob's `disk_request` training range is literally `[0, 0]` —
that corpus carries no per-task disk signal — so a kernel supplying a real disk fraction would place
*every* task outside the training box and the advisor would correctly abstain about the entire
machine. Aletheia therefore reports the field as unobservable (`missing_info`), which is true of it
today, and the column will only start carrying information when a corpus that has one is trained.
This was found by the live derivation landing, not by reading the paper.

### Context Engine — Context Fabric, not RAG

Aletheia understands its own world and provides the **smallest useful, authorized** context per task
rather than dumping data into the model. Layered retrieval, structured-first:

```text
intent → capability-aware retrieval
  ├── direct         subject · focus entity · held authority          [always]
  ├── structured     entity queries (type, properties, ownership)     [always]
  ├── relationships  world-model traversal from the focus             [always]
  ├── memory         relevant past actions                            [when relevant]
  ├── semantic       embeddings for ambiguous NL search               [OPTIONAL seam]
  └── knowledge      documents / transcripts / images                 [OPTIONAL seam]
→ rank / dedup / compress / budget → compact typed AiContext → model
```

Every entity is authorized (`entity.read`) **before** it enters context; a subject with no capability
gets no world context. Semantic/vector and document knowledge are optional interfaces — **no
embedding server or vector database is required** for normal OS operation.

## Boot it and use it

### 0. Which path works on your machine

The one thing that trips people up first, so it is first — check with `uname -m`:

| Your machine | Use | Why |
|---|---|---|
| **Apple Silicon Mac** (`arm64`) | **QEMU** — below | VirtualBox virtualizes the *host* architecture: it installs and answers `--version` on arm64, then fails at `startvm` for an x86-64 guest. |
| **Intel Mac / Windows / Linux** (`x86_64`) | QEMU **or** VirtualBox | VirtualBox is the no-QEMU path — see [Run it as an OS, in Oracle VirtualBox](#run-it-as-an-os-in-oracle-virtualbox). |

What you need:

```bash
brew install qemu mtools zstd                 # macOS (+ Xcode CLT)
sudo apt-get install -y qemu-system-arm qemu-system-misc qemu-system-x86 \
     ovmf mtools dosfstools zstd              # Debian/Ubuntu
```

Rust nightly is pinned; the toolchain file installs it for you on first build. If a build says
`can't find crate for core`, a Homebrew `cargo` is shadowing rustup's —
`export PATH="$HOME/.cargo/bin:$PATH"` fixes the session (every script in `scripts/` already does it).

### 1. Boot the OS and type at it

```bash
./scripts/run-interactive.sh aarch64     # or: riscv64, x86_64
```

The machine boots, runs its invariant suites in kernel space, and hands you a prompt — a real OS on a
real (virtual) machine, not a simulator of one:

```text
aletheia> help
aletheia> write manifesto the OS you can sit in front of
aletheia> ls
aletheia> grep front manifesto
aletheia> cp manifesto backup
aletheia> wc backup
aletheia> halt
```

27 commands over the namespace (`ls`, `cat`, `write`, `append`, `cp`, `mv`, `grep`, `hexdump`,
`find`, `wc`, `df`, `mem`, `reboot`, `halt`, …; type `help`), a real line editor — arrows,
`Home`/`End`, `Delete`, history on the up arrow, `Tab` completion over the names that exist. The disk
persists between runs at `kernel*/target/interactive-persistent.img`: boot again and `ls` still shows
what you wrote; delete that file for an empty namespace. **`Ctrl-A X` quits QEMU.**

Try all three CPU targets — it is the same `kernel-core` dispatcher on each.
[docs/BOOT.md](docs/BOOT.md) is the per-target boot reference (images, firmware, troubleshooting).

### 2. Let the model drive that console

The model does not get an API around the OS. It gets **the operator's surface with the operator's
authority**, and it cannot type a line you could not have typed. Get the weights and serve them —
**`--jinja` is not optional**, without it `llama-server` never parses a tool call and a correct answer
reads as no answer at all:

```bash
cd aletheia && cargo build --release && cd ..
./aletheia/target/release/aletheiad model status   # what is selected, and whether it is present
./aletheia/target/release/aletheiad model pull     # ~1.6 GB into the local HF cache

llama-server -m "$(./aletheia/target/release/aletheiad model status | sed -n 's/^weights: *present — //p')" \
             -c 8192 --port 8099 --host 127.0.0.1 --jinja
```

Ask it for one console line, then type that line at the prompt from step 1:

```bash
export MODEL_ENDPOINT=http://127.0.0.1:8099
printf '  objects:\n    30 manifesto\n    12 poem\n' > /tmp/brief.txt

./aletheia/target/release/aletheiad console plan --interpreter model \
  --context-file /tmp/brief.txt "show me the poem"
```

It prints one console line and nothing else, validated against the kernel's own command table. A
**destructive** request is different: it does not type and it does not refuse — it ASKS. The command
exits `7` with an approval id on stderr, the pending question is recorded durably in the Core's
approval store (ADR-015 applied to the console; ADR-059), and a human answers before anything is
typed — once:

```bash
./aletheia/target/release/aletheiad console plan --interpreter model \
  --context-file /tmp/brief.txt "remove the poem"        # exit 7, id on stderr
./aletheia/target/release/aletheiad approvals list       # see the question
./aletheia/target/release/aletheiad approvals grant <id> # or `deny` — both are records
./aletheia/target/release/aletheiad console plan --interpreter model \
  --context-file /tmp/brief.txt "remove the poem"        # now prints `rm poem`, exactly once
```

A yes binds to EXACTLY its line (`rm poem` says nothing about `rm manifesto`), is consumed at typing
time, and cannot survive replay. Or ask for a whole session, feeding the console's reply back as the
observation:

```bash
T=/tmp/session.json
./aletheia/target/release/aletheiad console agent --transcript $T --interpreter model \
  --context-file /tmp/brief.txt --approve \
  "make a copy of manifesto called backup, then tell me how big the copy is"

echo "manifesto -> backup (30 bytes)" > /tmp/obs.txt
./aletheia/target/release/aletheiad console agent --transcript $T --interpreter model \
  --context-file /tmp/brief.txt --approve --observation-file /tmp/obs.txt \
  "make a copy of manifesto called backup, then tell me how big the copy is"
```

Exit `0` with a line on stdout means *type this and come back*; exit `10` means it is done and
`answer:` on stderr is the answer. `corrected:` lines on stderr are proposals Aletheia refused and
re-asked — those never reached the machine. The bounds hold whatever the model proposes: a
destructive op without `--approve` is refused with **nothing on stdout** for a driver to type, and
stopping the machine is refused **even with** `--approve`.

### 3. Use the hosted Core

The System Core runs on the dev host behind the same Service API / IPC boundary applications use:

```bash
cd aletheia
cargo run                        # aletheiad demo: runs UC-001..004 as a CLIENT over the service boundary
cargo run -- serve               # long-running Core Alpha behind the Unix-socket IPC boundary

cargo run -- model list          # what this machine actually has; * marks the running selection
cargo run -- model use lfm2.5    # switch (unique prefix is enough); persisted under $HOME/.aletheia
cargo run -- model status        # confirms the backend is serving the model you selected
cargo run -- model bench         # every registered operation through the real model; non-zero on any failure
MODEL_ENDPOINT=http://127.0.0.1:8099 cargo run   # provider healthy → the model interprets intents
```

With no model available at all the **deterministic interpreter** takes over and the OS stays fully
functional — nothing above requires a resident model.

[docs/TRY-IT.md](docs/TRY-IT.md) is the long-form operator walkthrough of steps 1–2, and
[docs/MATURITY.md](docs/MATURITY.md) grades every subsystem and says plainly that nothing here is
production-ready.

### Run it as an OS, in Oracle VirtualBox

No QEMU, no OVMF, no `mtools`, no WSL — VirtualBox, Rust and Python 3 are enough, on **Windows,
macOS or Linux**:

```bash
./scripts/vbox-install.sh                 # build the image + install a persistent VM named "Aletheia"
./scripts/vbox-install.sh --interactive   # ...the build that hands the machine to a console you can type at
VBoxManage startvm Aletheia               # watch it boot; the serial log is the machine-checkable verdict
```

The VM it provisions has **2 vCPUs and 512 MiB**, which is what this OS needs rather than what was
convenient to type: the VirtualBox gate boots the same image at that size on every run (`MEM_MB` and
`CPUS` override it). The interactive build gives you a console with a real line editor — arrows,
`Home`/`End`, `Delete`, a history walked with the up arrow, `Tab` completion over the names that
exist — and 27 commands over the namespace (`ls`, `cat`, `write`, `append`, `cp`, `mv`, `grep`,
`hexdump`, `find`, `wc`, `df`, `mem`, `reboot`, `halt`, …; type `help`). It works from the VM
window's own keyboard as well as over a host serial pipe.

`kernel-x86_64/scripts/mkesp.py` writes the bootable GPT/ESP image with nothing but the Python
standard library, so the same artifact comes out byte-identical on all three hosts.
[docs/VIRTUALBOX.md](docs/VIRTUALBOX.md) is the full walkthrough — reading the boot log, the
interactive shell over a host serial pipe, and what VirtualBox *cannot* cover.

**Why two hypervisors.** A kernel that boots only on QEMU has proved "correct against QEMU": the
emulator and the kernel can be wrong together, and more QEMU testing cannot find it. VirtualBox
brings its own EFI, its own ACPI tables, SATA/AHCI (**no virtio-blk**) and no `isa-debug-exit`, so
the verdict there is the serial log with marker parity against the QEMU gate, and the four device
families it cannot emulate are printed as SKIP rather than quietly dropped. Adding the rung caught
two real defects on its first runs — an invariant that had encoded OVMF's memory map, and a
mis-declared kernel-image extent that only overlapped the user region at a larger guest RAM size
(the gate now boots at two memory sizes for exactly that reason). See
[ADR-046](docs/adr/ADR-046-second-hypervisor-qualification-virtualbox.md).

Configuration (all optional; defaults shown):

```text
AI_PROVIDER=local            # or "deterministic" to force the fallback interpreter
MODEL_BACKEND=llama_cpp
MODEL_ENDPOINT=http://localhost:8080
MODEL_REF=LiquidAI/LFM2.5-2.6B-GGUF    # from the selected manifest; setting it leaves the registry
# MODEL_PATH=/abs/path/to/model.gguf   # explicit override; otherwise resolved from the HF cache
# ALETHEIA_MODEL=minicpm               # use a REGISTERED model for one run without persisting a switch
# ALETHEIA_LM_MODEL=/path/to/aletheia-lm.gguf   # where `model use aletheia-lm` will look, once trained
```

Resolution is an order, not a merge: **environment** beats the **persisted selection**, which beats
the manifest marked `default`. Setting `MODEL_REF` detaches the configuration from the registry — the
model is then reported as `(unregistered)` and `model bench` refuses to run, because a model whose
identity Aletheia cannot verify must not be measured under a registry name.

### Verify it yourself

Booting and using it is the point above; none of it needs these. When you want to check rather than
believe, every number this repository quotes has a gate behind it:

```bash
cd aletheia && cargo test        # full conformance + unit suite (deterministic; no model needed)
cd kernel-core && cargo test     # the shared substrate's hosted proofs — 452 tests incl. soak + the self-benchmark suite
./scripts/e2e-all.sh             # all three CPU targets + the VirtualBox rung, one aggregate pass/fail
./scripts/console-e2e.sh         # boot, type, halt, reboot, read it back — three targets
./scripts/comparative-bench.sh   # Aletheia vs a real Linux kernel on the SAME emulator:
                                 # boot, idle CPU, and a typed echo-workload leg under load
```

Two of those proofs deserve naming. The **long-running soak**
(ADR-063): every suite was proved on hand-picked cases; the soak asks whether the properties
survive the machine RUNNING — committing, naming, sharing and dispatching for a long time. On a kernel
whose heap is a bump allocator that never frees, that is a resource property before it is a correctness
property, so the load-bearing check is MEASURED on the machine itself: journal churn must allocate
**nothing per transaction**, held exactly against each target's own heap meter, while namespace
mutations are structurally audited after every operation, capability grants are churned through
share→attenuate→write→read→revoke with every refusal counted, task generations prove a Finished task
never runs again — and the same seed must replay the identical campaign. From a real boot:

```text
[soak] journal: 396 txs (120 verifies, 3 recovers replayed) in 35 ms => 11096 tx/s
[soak] namespace: 12 ops, every one audited => 3158 ops/s, 3 survivors re-mounted
[soak] grants: 96 cycles, 96/96 unauthorized refused, 288/288 revoked accesses refused
[soak] tasks: 48 generations, 768 priority dispatches, each exactly-once
[soak] heap: 1669872 B used by the whole campaign (bump allocator never frees)
[soak] ALL 12 SOAK INVARIANTS HOLD
```

Throughput is reported, never gated — QEMU-TCG nanoseconds are an emulator's numbers. The properties
are gated, on all three CPU targets, in CI.

The **self-benchmark** (ADR-064) is its performance sibling: an arch-independent suite, defined once
in `kernel-core/src/bench.rs`, runs inside every target's boot gate and measures the five load-bearing
paths on each machine's own clock — authority checks, capability-checked delivery, journal commits,
scheduler dispatches, console formatting. Throughput is again REPORTED (nanoseconds where the clock is
calibrated; raw ticks labelled "uncalibrated" on x86-64, whose TSC frequency nobody here pretends to
know); what is GATED is everything structural: work really done, authority unbroken mid-window, commits
read back byte-for-byte, steady state proven by a zero-materialization rerun window, exactly-fair
dispatch, byte-exact console arithmetic, identical-work determinism — and four pixel-level checks that
render THIS boot's summary onto real framebuffer pages through wrap and scroll, so every number is
verified on BOTH consoles: the serial log a gate judges, and the display a human sees. From a real boot:

```text
[bench] authority : 100000 checks | 15 ms | 6384476/s | 144 ns/op
[bench] delivery  : 100000 rt | 33 ms | 3018684/s | 320 ns/op
[bench] storage   : 256 txs | 20 ms | 12570/s | 79536 ns/op
[bench] schedule  : 100000 disp | 40 ms | 2479604/s | 400 ns/op
[bench] console   : 100000 lines | 6 ms | 16556291/s | 48 ns/op
[bench] ALL 12 BENCHMARK INVARIANTS HOLD
```

And the comparative benchmark now drives a **typed workload** into both guests on the same emulator —
identical paced `echo` round-trips, judged by output tokens, wall-clocked end-to-end — so the cross-OS
comparison covers how each system ANSWERS under load, not only how it boots and how it waits.

[docs/TRY-IT.md](docs/TRY-IT.md) §3 lists the rest (per-target VM gates, the model-driven console
gate, conformance, and the three claim-checking gates that assert the evidence exists and that CI
actually runs it).

## Research

[docs/research/RUST-OS-DEEP-RESEARCH.md](docs/research/RUST-OS-DEEP-RESEARCH.md) is the survey this
architecture is argued against rather than asserted over: the 2026 Rust-OS field (Redox, Asterinas's
framekernel, Theseus, seL4's verification economics, Rust-for-Linux), precisely what Rust buys a
kernel and the four places the guarantee leaks, the Kani/Verus/Miri verification landscape, and the
2026 agent-OS literature — which converged independently on the two decisions this repository made
first: **no ambient authority**, and **authority separate from governance**. It closes with five
vindicated decisions and six exposures, each mapped to an open row in the gap register.

## Status

See [STATUS.md](STATUS.md) for the delivered milestones and test counts, and
[docs/](docs/) for the PRD, SAD, and ADRs (ADR-015 policy/approval separation, ADR-016 Service
API/IPC boundary, ADR-017 AI subsystem, ADR-018 Context Engine, ADR-021 SMP, ADR-027 capability
concurrency, ADR-029 mapping-API admission check, ADR-030 frame ownership, ADR-031 page-table reclamation, ADR-032 address-space destruction, ADR-033 erase on free, ADR-034 W^X, ADR-062 fault
injection, ADR-063 long-running soak). Open findings — what is
NOT claimed — live in [docs/gap/ARCHITECTURE-GAPS4-REGISTER.md](docs/gap/ARCHITECTURE-GAPS4-REGISTER.md).

## Strategic path

```text
macOS-hosted Rust prototype
  → platform-independent Aletheia architecture
  → native Rust-first Aletheia kernel and system runtime
  → completely standalone Aletheia operating system
```

The hosted implementation preserves the security and semantic concepts as the actual foundation of a
new OS — not a demo, and not built on Linux.
