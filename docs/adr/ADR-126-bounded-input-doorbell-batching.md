# ADR-126 — Input receive reposts are batched with a bounded quiescence flush

**Status:** Accepted · **Date:** 2026-09-09 · **Advances:** ALET-P2-021 performance/GUI rung · **Builds on:** ADR-080, ADR-085, ADR-086, ADR-125

## Decision

The virtio-input receive path restores consumed event buffers without issuing one MMIO device
notification for every event. `REPOST_KICK_BATCH` is fixed at four: the first three restored
buffers remain pending and the fourth restore issues the notification and clears the pending count.

The batching is bounded in both directions. If a device becomes quiescent before four buffers have
been restored, the next empty used-ring poll flushes the smaller pending batch. A short burst can
therefore never leave replacement buffers unnotified indefinitely. Initialization still issues the
initial queue notification, so the live device starts fully armed.

`VirtioInput::doorbells()` exposes the notification count as machine telemetry. The desktop's
`input` facts report keyboard and pointer notification counts alongside event counts, making the
MMIO reduction observable on the live machine rather than inferred from source inspection.

The terminal input queue is also `VecDeque<u8>` so each consumed byte is removed in O(1) rather than
shifting the remaining queue contents.

## Security and performance consequences

- DMA validation and registration occur exactly as before, before restored descriptors are published.
- The notification batch is structurally bounded to four; no unbounded refill debt is introduced.
- Short bursts are flushed at observed quiescence, preventing refill starvation.
- Bursty input reduces event-queue notification traffic and associated MMIO work.
- Terminal byte consumption changes from O(n) front removal to O(1) `pop_front` without changing
  capacity or overflow semantics.
- Notification counters provide live evidence for benchmark and E2E analysis.

## Verification

- `cargo test --manifest-path kernel-core/Cargo.toml --test vinput` — 10 passed.
- `cargo test --manifest-path kernel-core/Cargo.toml` — full suite running/verification required
  before this wave is committed.
- `git diff --check` — clean before this documentation update.

## Non-claims

This does not close ALET-P2-021 or ALET-P2-022. It does not claim hardware-level frequency or
voltage control, GPU isolation, or lower latency on physical hardware until those properties are
measured on the corresponding hardware.
