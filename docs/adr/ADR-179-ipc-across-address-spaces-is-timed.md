# ADR-179 — IPC across address spaces is timed, against Linux, on the same emulator

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-PERF-005 (new)
**Builds on:** ADR-064 (the machine measures itself), ADR-056 (the honesty rule), ADR-171 (a
second operating system).

## Context

The boot `bench` suite timed an in-kernel message round trip (336 ns on aarch64) and said, in its
own output, that this "does NOT show Aletheia IPC < Linux IPC": the loop crossed no privilege or
address-space boundary. `docs/BENCHMARKS.md` listed a real cross-address-space benchmark as an
open improvement, because without one the Linux pipe baseline (22 us in Docker's hardware VM)
could not be set against anything.

## Decision

* **Aletheia side.** Each target's user-mode suite gains `run_ipc_pingpong(1000)`: two user
  processes in separate address spaces exchange 1000 round trips through the kernel endpoint. One
  round trip is A sends, B receives, B sends the reply, A receives: four user-mode entries, four
  syscall traps and eight page-table-root switches (CR3 / TTBR0 / satp). One Trial serves every
  excursion, and SEND/RECV authorize on the capability engine's Allow arm, which allocates
  nothing. The number is REPORTED (`[bench] ipc: ...`), never gated; what is gated is one new
  invariant per target: every body crosses intact, and the heap does not move across the timed
  loop (`usermode` 32 -> 33 on aarch64 and riscv64, 39 -> 40 on x86-64).
* **Linux side.** `comparative-bench.sh`'s initramfs gains a static `pingpong` (two processes, two
  pipes, 8-byte messages, the same warm-up and 1000 round trips), typed into the same guest that
  ran the echo workload. The results table gains an "IPC round trip, 2 spaces" row.

## Results (x86-64, `qemu-system-x86_64`, TCG, same host, same flags)

| | Aletheia (kernel endpoint) | Linux 6.12 (pipes) |
|---|---|---|
| IPC round trip between two address spaces | 22,134 ns | 41,051 ns |

Per-target Aletheia numbers at boot: x86-64 21.9 us, aarch64 27.5 us, riscv64 41.7 us.

## What this does and does not show

It is a narrow microbenchmark, and the two sides do not do the same work:

* Aletheia pays MORE boundary crossings per round trip (eight page-table switches against Linux's
  two context switches).
* Aletheia's endpoint is a single-slot mailbox holding one register-sized body. Nothing is copied
  and no scheduler decides who runs next: the suite drives the two processes itself. Linux copies
  through the pipe buffer, runs VFS, and wakes the reader through its scheduler.

So the row says Aletheia's capability-checked kernel crossing is cheap under TCG. It does not say
Aletheia has faster IPC than Linux for any real workload: a scheduled, buffered, multi-message
endpoint would pay costs this loop does not.

## Consequences

**Good.** The comparison `bench` has disclaimed since ADR-064 now exists, measured the same way on
both systems, with its asymmetries written next to it.

**Costs.** Boot time grows by the loop (about 22-42 ms per target under TCG).

**Not claimed.** Hardware numbers, a scheduled or blocking endpoint in the timed loop, or messages
larger than one word.
