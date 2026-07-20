# Aletheia Microkernel Reference (bare-metal aarch64)

A `no_std` Rust microkernel that boots on QEMU `virt` and re-proves the M1 System-Core
invariants **in kernel space**, on real privilege (EL1), instead of in a hosted userspace
process. This is the first concrete step of PRD phase **P4** (real microkernel, VM-tested)
and it is done **contract-honest** per ADR-010: everything here is measured and executed,
nothing is asserted by documentation alone.

## What it demonstrates

1. **Boot** on QEMU `virt` at EL1: stack + BSS + heap, PL011 UART, exception vectors.
2. **The capability-secure spine in kernel space** (`src/spine.rs`): a content-addressed
   semantic store, a capability engine with an **unforgeable-by-construction** `CapToken`
   (its id field is module-private — no code outside `spine` can fabricate one), and the
   `validate → authorize → execute → verify → record-event` pipeline.
3. **11 invariant selftests** (`src/selftest.rs`) — the M1 acceptance criteria, re-proved
   live against the real in-kernel spine. The first failing check sets the VM exit code.
4. **Performance validation** (`src/bench.rs`): the IPC fast-path latency vs the raw
   syscall floor, measured on the same emulated CPU.

## Run it

```bash
# one-shot end-to-end VM test (build + boot + assert + exit code) — the CI gate
../scripts/vm-e2e.sh

# or just boot it and watch the serial output
cargo run

# real-Linux IPC baseline for the perf discussion (needs Docker)
../scripts/linux_pipe_bench.sh
```

`cargo run` uses the QEMU runner in `.cargo/config.toml`; the kernel exits the VM via
semihosting, so the process exit code IS the verdict:

| exit code | meaning |
|-----------|---------|
| `0`       | all invariants held — e2e PASS |
| `10 + i`  | invariant `i` failed |
| `101`     | kernel panic |
| `102`     | unexpected CPU exception |

## Toolchain

Nightly Rust with `rust-src` (for `-Z build-std`, since no precompiled `core`/`alloc` ships
for the bare-metal target) and `qemu-system-aarch64`. Both are pinned/declared:
`rust-toolchain.toml` and `.cargo/config.toml`.

## Performance honesty (read before quoting numbers)

- The in-VM figures are **wall-clock ns under QEMU TCG emulation**, not bare-metal hardware
  timings. Do not present them as hardware latency.
- The **`svc` syscall floor and the capability-check number are measured in the same run on
  the same emulated CPU**, so their *ratio* is substrate-fair. The defensible claim is
  **narrow**: the capability authorization check Aletheia *adds* costs less than one bare `svc`
  trap (the two-check request/response measures ≈0.79× one `svc`). The added authority check is
  cheap.
- This does **NOT** establish that Aletheia IPC is faster than a Linux pipe. The measured loop
  is two `evaluate()` calls plus a ring push/pop, run entirely in EL1 — it crosses **no**
  privilege or address-space boundary. A *real* microkernel IPC round-trip **and** a Linux pipe
  both pay ≥2 boundary crossings plus context/address-space switches, which this loop skips. So
  the benchmark isolates the cost Aletheia *adds* (the authority check), not IPC transport. A
  microkernel's real edge is a short in-kernel code path, which this reference does not yet
  measure.
- Next performance-validation milestones: cross-address-space IPC, and a comparison against a
  Linux guest running in the **same** emulator (so the substrate matches). Only then can an
  Aletheia-IPC-vs-Linux-IPC claim be made honestly.

## Not yet here (later P4/P5 phases)

Virtual memory / per-actor address spaces, preemptive scheduling, real cross-AS IPC with
page-granted transfer, secure boot, and hardware-lab testing. Those need architecture text
and staged, measured implementation — not blind code.
