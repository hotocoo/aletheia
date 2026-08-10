# ADR-056 — An idle machine should cost nothing, and a comparison should be run on one substrate

**Status:** Accepted
**Date:** 2026-08-10
**Requirements:** REQ-CON-006 (an idle console waits instead of spinning), REQ-PERF-001 (comparative
benchmark against a real Linux kernel on one substrate)
**Extends:** ADR-044 (interactive console), ADR-045 (interrupt-driven console input), ADR-054/055
(the console agent loop)

---

## Context

The console agent gate reported a median of **7.8–9.9 seconds per turn** against the resident model.
Run standalone against the same backend, the same turn took **1.2–1.8 seconds**. The model had not
changed, the prompt had not changed, and the backend had not changed. The obvious suspect was that
during the gate there were four emulated guest vCPUs running next to it.

**That suspicion was wrong, and this ADR records it as wrong rather than deleting it**, because the
investigation it started found a real defect anyway. After the fix below took an idle guest from
91.8% of a host CPU to 0.9%, the gate's medians were re-measured: **8567 / 7204 / 8172 ms**, against
9877 / 7849 / 7768 before. Unchanged inside the noise. Contention was not what made the gate slow.

What made it slow is the model, and `REQ-AI-010`'s per-call token accounting is what said so. A turn
that ends in a tool call costs ~224–353 completion tokens and 1.7–2.3 s. A turn that ends in an
ANSWER costs **859–1121 completion tokens** to produce one short sentence, because LFM2.5 emits a
long `reasoning_content` first — and a turn that was corrected pays for both. The prompt is ~2000
tokens and is prefix-cached, so prefill is not the cost. **Aletheia's own overhead is not measurable
next to this.**

One optimisation was tried and is recorded as a NEGATIVE result so nobody spends the afternoon on it
twice: sending `chat_template_kwargs: {enable_thinking: false}` for this model changes nothing.
Completion tokens were identical (296 / 224 / 353) with and without it. `models/lfm2.5.toml` keeps
`thinking = false` for that reason.

`kernel_core::shell::run_loop` read:

```rust
loop {
    let Some(byte) = getc() else { continue };
```

A machine sitting at a prompt with nobody typing therefore asked the input ring for a byte, was told
there was none, and asked again — forever, on every core. Measured on this host, an Aletheia x86-64
guest parked at an idle prompt cost **91.8% of a host CPU**.

This was invisible for as long as nothing else on the machine mattered. It stopped being invisible
the moment Aletheia's own intelligence became the other thing on the machine: the model deciding
what to type next was competing with the guest that was waiting to be typed at.

It was also simply wrong, and had been wrong since ADR-045. Console input arrives by **interrupt**
(REQ-CON-002). The loop already had something to wait for.

## Decision 1 — `ShellHost::idle`, defaulted to doing nothing

`run_loop` calls `host.idle()` when the ring is empty. Each target implements it with the
instruction that parks the CPU until an interrupt arrives:

| Target | Instruction | What wakes it |
|---|---|---|
| aarch64 | `wfi` | UART IRQ through GICv2. An earlier draft of this table also claimed the generic timer PPI as a second wake source; it is armed by the preemption suite, which runs *before* the interactive handoff, and whether it is still enabled at the prompt was never checked — so the claim is withdrawn rather than left standing |
| RISC-V | `wfi` | UART external interrupt through the PLIC (a spurious `wfi` return is permitted and the surrounding loop simply asks again) |
| x86-64 | `sti; hlt` | UART interrupt on IRQ4 through the 8259A |

On x86-64 the ordering carries the entire correctness argument: `hlt` with interrupts masked is a
machine that never wakes, and `sti` has a one-instruction interrupt shadow, so the pair cannot lose
an interrupt arriving between them.

**The default implementation does nothing at all**, and that default is the safety argument. A target
whose console is polled rather than interrupt-driven would never be woken, and parking such a machine
forever is a far worse failure than spinning on it. A target opts in only by implementing `idle`,
which is a statement that an interrupt will arrive — and `scripts/console-e2e.sh` is what proves the
statement, because a target that got it wrong stops responding to the very first thing typed at it.

**Measured, this host** (x86-64 UEFI image, `qemu-system-x86_64`, TCG, `-smp 4`, idle at a prompt):

| | host CPU |
|---|---|
| before | 91.8 % |
| after | 0.9 % |
| Linux 6.12-lts, same emulator, same flags | 1.1 % |

All three targets still PASS `scripts/console-e2e.sh`.

## Decision 2 — a comparison is only a comparison on one substrate

`scripts/linux_pipe_bench.sh` already stated the problem and then lived with it: Aletheia's kernel
numbers come out of QEMU TCG and a Linux container's come out of near-native execution, so comparing
those wall-clocks compares emulators. Every "faster than Linux" claim made that way is measuring the
hypervisor.

`scripts/comparative-bench.sh` removes the objection instead of restating it. **Both** systems boot
under the same `qemu-system-x86_64`, on the same host, with the same `-machine`, `-m`, `-smp` and
`-cpu`, in the same TCG mode, to the same end state: an interactive shell on `ttyS0` blocked on
input. The Linux leg is a real Linux 6.12-lts kernel with a busybox initramfs whose `/init` prints a
marker and `exec`s a shell — a distro booting into a service manager would be measuring the distro.

**Measured, this host:**

| | Aletheia (x86-64) | Linux 6.12-lts |
|---|---|---|
| boot to interactive shell | 4068 ms | **2053 ms** |
| idle host CPU at prompt | **0.9 %** | 1.1 % |
| bootable payload | **522 752 B** | 13 895 207 B |
| privileged lines of code | **22 083 (Rust)**, 302 `unsafe` occurrences | ~40 M (C, cited for scale, not measured) |

**Linux boots faster, and this ADR records that the script's own commentary predicted the opposite
before the first run corrected it.** Most of that gap is not the kernels: Aletheia boots through
OVMF, a full UEFI firmware implementation, while the Linux leg is loaded directly by QEMU with
`-kernel` and skips firmware entirely. That is a real difference in what an operator waits for and it
is Aletheia's to own, but it is a boot-*path* difference rather than evidence that the kernel is
slow.

The two columns worth arguing about are the last two. **Idle CPU** is a fair fight — identical work
(none), identical emulator — and parity with Linux is the claim. **Privileged lines of code** is the
only column that depends on no emulator, no host and no workload: it is how much code must be correct
for the machine to be correct, and it is where the design rather than the youth is doing the work.

The script prints what Aletheia loses, in its own output, because a benchmark that omits that is
advertising.

## Consequences

**Good.** An idle Aletheia costs what an idle Linux costs, and a machine that is waiting for its
operator is no longer competing with the model deciding what to type. Per-call token accounting makes
"the turn was slow" a diagnosis rather than an observation: a re-prefilled prompt and a long
generation are different problems with different fixes. Comparative claims have a script behind them
that anyone can re-run, including the parts where Aletheia loses.

**Explicitly not delivered by this ADR.** Agent turn latency is unchanged, because it was never
Aletheia's to fix — it is a 2.6 B Q4 model's generation time on this workstation.

**Costs.** `idle` is a per-target `unsafe` asm block, three more of them. The Linux leg needs Docker
and network access and SKIPs loudly without them.

**Not claimed.** Nothing here says Aletheia is a faster operating system than Linux, and the boot
column says the opposite. `docs/MATURITY.md` governs every claim in this repository.
