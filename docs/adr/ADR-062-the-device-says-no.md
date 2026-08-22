# ADR-062: The device says no

**Status:** Accepted · **Date:** 2026-08-22 · **Closes:** ALET-P2-008 (REQ-QUAL-006); delivers a slice of ALET-P1-019 · **Builds on:** ADR-013

## Context

The journal's contract is all-or-nothing across a crash, and its existing proofs exercised crash
points by snapshotting the device between protocol steps. What no proof had done is let the DEVICE
itself refuse mid-protocol — a write that never lands, a flush whose barrier does not hold, a read
that comes back as an error — at every position of the protocol, and hold the caller to its
contract under all of them. Fault injection was listed in the register precisely because real
devices refuse, stall, and fail DURABILITY BARRIERS, not just return clean snapshots.

## Decision

`kernel-core/src/faultdev.rs` adds `FaultInject<D>: BlockDevice` — a scripted adversary that pops
an operation program (`ReadOk/ReadFail/WriteOk/WriteFail/FlushOk/FlushFail`) and passes through
once spent. Refusal semantics are stated once and proved:

* a refused WRITE mutates nothing;
* a refused READ leaves the caller's buffer UNWRITTEN (no zeroing fiction);
* a refused FLUSH means the durability barrier did not hold — surfaced as `Err(Device)`, because
  reporting success there would be durability that does not exist.

Against that adversary, `kernel-core/tests/faults.rs` proves, EXHAUSTIVELY over the swept
transaction's whole op sequence:

1. **A refusal at every commit position ends in exactly one promised world.** Eight positions of a
   two-update commit, one refusal each: after recovery the home blocks are ALL-old or ALL-new,
   never mixed — and which one is decided by the PIVOT rule (position of the commit-record flush):
   pre-pivot refusals end old with nothing replayed; post-pivot refusals end new through recovery.
2. **A failed flush surfaces.** The journal returns `Err(Device)`; it never reports a transaction
   durable when its barrier failed.
3. **Recovery survives a refusal at each of ITS positions too**, and a retry on healthy hardware
   COMPLETES: the committed transaction is still found and replayed to the new world everywhere —
   the idempotence claim is now load-bearing and tested, not asserted.
4. **Refusals carry no fiction**: buffer untouched on refused read; script exhaustion restores
   pass-through byte-for-byte, so post-fault assertions are about the PROTOCOL, not the adversary.

## Consequences

* `FaultInject` composes with anything taking `BlockDevice` (journal, filesystem, persist) without
  those layers knowing an adversary exists; future protocols inherit the harness for free.
* The sweep is exhaustive over the swept protocol's positions, not sampled — adding a step to the
  commit ordering grows the proof with it.
* Scope stated: single-threaded scripting (the trait's `&self` read path needs interior
  mutability); the live virtio-blk driver's error paths remain covered by its own gate suite — this
  ADR arms the HOSTED side of ALET-P1-019, whose driver-depth half stays open.
