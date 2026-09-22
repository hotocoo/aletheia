# ADR-150 — A completion budget measured in time, not in looks

- **Status:** accepted
- **Date:** 2026-09-22
- **Requirement:** REQ-DRV-007 (extends REQ-DRV-003, the virtio-blk driver)
- **Supersedes:** the spin-count bound in `virtioblk::submit` (ADR-040's driver, tightened per kick
  by ADR-073 and ADR-074's probe suites).

## Context

`VirtioBlk::submit` waited for a request's completion by polling `used.idx` up to fifty million
times, then declared the device dead (`StorageError::Device`). On an idle machine that is seconds
of waiting and no healthy device comes near it. On 2026-09-22 the aarch64 boot gate on the GitHub
runner failed twice in two consecutive runs, in two different places — once as
`[persist] PERSISTENT MEDIUM FAILED: Fs(Storage(Device))`, once as
`fs: two objects never share a data block` (a `create` that failed) — and passed on re-run both
times. The same code passes every time locally.

Both are one fault. A spin count measures how fast THIS CPU can look at a memory location; it says
nothing about how long the device has had. Under TCG on a shared, loaded runner the guest CPU
polls fast while QEMU's block backend waits on a starved host thread, so a bound that was
"seconds" on an idle machine became "a few hundred milliseconds" on a busy one, and a healthy disk
was refused as dead.

## Decision

- **`VirtioHal` gains `now_ns()`**, a monotonic clock in nanoseconds, from each target's existing
  counter (`cntvct` on aarch64, `rdtime` on RISC-V, `rdtsc` on x86-64 — the same tick every latency
  figure in this tree already uses).
- **The wait is bounded on that clock.** `SUBMIT_BUDGET_NS` is twenty seconds: a healthy device
  answers in microseconds and a starved emulator in well under a second, so only a device that will
  never answer exhausts it, and a device that answers late is a slow device, not a dead one.
- **Probe callers name their budget in time too.** The VT-d and SMMUv3 suites deliberately provoke
  lost completions and pay one timeout per kick; they now set `PROBE_BUDGET_NS` (half a second) per
  kicking device instead of four million looks.
- The used-ring `id` validation, the exact-length rule and every other completion check are
  unchanged: this ADR changes how long the driver waits, not what it accepts.

`virtq::poll_used_bounded` (the multi-queue substrate under net, gpu and input) keeps its poll
count on purpose: there the caller is waiting for an event that may legitimately never come — a
packet, a keypress — and chooses how many times to ask. A block request the driver itself issued
is a different contract: the device owes an answer, and the question is only how long to wait.

## Proof

Host (`kernel-core/tests/virtioblk.rs`): the host stand-in's clock is a counter that advances by a
chosen step per look. A silent device is refused once the clock passes the budget (the default is
the twenty seconds the contract names); with a clock that leaps past the whole budget on its first
tick, the refusal comes after a single look — a spin count of one could never be a budget, so the
bound can only be the time. Every other host and boot invariant over the driver is unchanged and
still holds on all three CPUs.

## Alternatives considered

**Raise the spin count.** Rejected: any count is a guess about the ratio of guest poll speed to
host I/O latency, and that ratio is exactly what a loaded runner changes.

**Re-run the job.** Rejected as a fix: it is how the fault was found, and it hides the class of
failure this tree exists to name.

## Consequences

A boot on a starved host now waits for its disk instead of misreporting it. The boot watchdogs
(240 s) remain the backstop above the budget.
