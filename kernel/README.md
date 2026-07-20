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
- The **`svc` syscall floor and the Aletheia IPC number are measured in the same run on the
  same emulated CPU**, so their *ratio* is substrate-fair. The defensible claim is the ratio:
  a full capability-checked IPC round-trip costs *less than one* bare privilege-boundary
  crossing, while a Linux pipe round-trip must pay **at least two** crossings plus a context
  switch and buffer copies. On identical hardware the microkernel IPC fast-path therefore has
  the lower floor.
- This measures the fast-path **within one address space** (the reference kernel is
  single-AS today). Cross-address-space switch cost is **not** modeled yet; the `svc` floor
  is reported precisely so the comparison is not silently flattering. A full cross-AS IPC
  benchmark and a same-emulator Linux guest comparison are the next performance-validation
  milestones (tracked in the PRD V&V section).

## Not yet here (later P4/P5 phases)

Virtual memory / per-actor address spaces, preemptive scheduling, real cross-AS IPC with
page-granted transfer, secure boot, and hardware-lab testing. Those need architecture text
and staged, measured implementation — not blind code.
