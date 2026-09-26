# ADR-198 — The kernel heap frees

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-MM-009 (new)
**Supersedes:** ADR-063's "the heap never frees" posture.
**Builds on:** ADR-197 (`kheap`, proved on the host).

## Decision

`kernel_core::kheap` is the `#[global_allocator]` on aarch64, riscv64 and x86-64: one spin lock,
taken with interrupts masked on the current CPU (DAIF.I / sstatus.SIE / `cli`), because the desktop's
pump allocates from the timer interrupt and must never spin on a lock its own CPU holds.

`heap::used_bytes()` keeps its old meaning — every byte ever handed out, never decreasing — so every
storm that proves "this path allocates nothing" by a watermark still proves exactly that. `mem` now
reports LIVE bytes and bytes actually available (`heap: X B used, Y B free`), no longer "(never
freed)".

## Proof

* Boot: `kheap=5` on all three CPUs (the suite over a 256 KiB region lent from the heap itself).
* Host: the 200,000-operation seeded workload (ADR-197).
* Live, aarch64 at 2560x1440: twelve alternating `resolution` switches all succeed; live heap stays
  at 2.3-2.8 MB and free heap at 13.6-14.1 MB throughout (before this wave the fifth switch was
  refused with 5.5 MB left). The ADR-196 heap floor stays as a safety bound and no longer binds.
* All boot, desktop, fuzz (console, network), crash and quality gates pass.

## Non-claims

No per-CPU caches (one lock serializes allocation); over-aligned (> 4 KiB) large blocks are not
reclaimed; no defragmentation.
