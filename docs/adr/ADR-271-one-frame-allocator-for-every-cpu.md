# ADR-271: One frame allocator for every CPU

**Status:** Accepted (2026-11-19)
**Requirements:** REQ-MM-001, REQ-MM-002, REQ-MM-005 (unchanged; one implementation now proves them)
**Builds on:** ADR-030 (frame ownership), ADR-033 (erase on free). Closes the `frames.rs` part of
ADR-263's "near-copies" item.

## Context

`kernel/src/frames.rs`, `kernel-x86_64/src/frames.rs` and `kernel-riscv64/src/frames.rs` each carried
the same 200-line physical frame allocator (intrusive LIFO free list, ownership table, zero on
allocate, erase on free), identical except for the handle's name (`PhysFrame` on aarch64 and
riscv64, `Frame` on x86-64) and the visibility of its methods. A fix to one copy had to be made three
times, and the only host proof of the rules was `frameown`'s, not the allocator's.

## Decision

* `kernel_core::framealloc` holds the allocator, `PhysFrame` (with `from_addr` for the
  address-taking free and the selftests' deliberately bad handles) and `FRAME_SIZE`. Each target keeps
  only what differs: its memory map (`RAM_BASE`/`RAM_END`, or the UEFI map on x86-64), its ownership
  state array, the single-core global, and its boot selftest. x86-64 re-exports the handle as `Frame`,
  so no caller changed.
* `kernel-core/tests/framealloc.rs` proves the allocator on the host: every frame handed out once,
  then a refusal; a double free, a foreign owner's free, a misaligned and an out-of-pool address all
  refused with counts unchanged; erase on free and zero on allocate; an ownership table too small for
  the pool refused.

## Evidence

* The three boot gates (`vm-e2e.sh`, `vm-e2e-riscv.sh`, `vm-e2e-x86.sh`) and `conformance.sh` pass
  on the shared allocator: every `frames:` invariant and every family above it (memory, virtual
  memory, user mode, reclaim) allocate through it.
* The three `frames.rs` lost 595 lines and `kernel-core` gained 231 (-364 net); the unsafe audit moves
  8 sites from each target to one place.

## What this is not

* `desktop.rs` and `heap.rs` stay per target: what differs there is the point (how each CPU masks
  interrupts around the desktop, where each linker puts the heap), and the shared parts already live
  in `kernel_core::desktop` and `kernel_core::kheap`.
* Still single-core: the global wrapper's `Sync` relies on no preemption during allocation, as before.
