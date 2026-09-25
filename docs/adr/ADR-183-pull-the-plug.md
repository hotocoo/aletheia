# ADR-183 — Pull the plug

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-FS-003 (new)
**Builds on:** ADR-069/072 (durable, crash-proved storage), ADR-088 (the filesystem under storm).

## Context

The journal's crash consistency is proved on the host by injecting a crash at every recorded
device operation, and the persistence gates reboot cleanly and read back. Nothing killed a RUNNING
machine mid-write and looked at what the real virtio-blk device kept.

## Decision: `scripts/crash-e2e.sh`

On one persistent disk image, `CRASH_ROUNDS` times per CPU (aarch64, riscv64, x86-64): boot the
interactive console; require the namespace to mount; `cat` objects `o0..o7` and require each to hold
exactly one complete version that was legal at the crash (version `v` of object `k` is
`v<v>-k<k>-` plus filler whose length also encodes `v`, so a torn object matches no real version);
then type a stream of whole-object rewrites and removals and SIGKILL QEMU at a random moment. An
object whose mutation was in flight may be in its old or its new state; an answered mutation fixes
it exactly. The gate fails if no object was ever present, so it cannot pass vacuously.

## Result

12 crashes per CPU, 6,000-15,000 versions issued, 104 object checks per CPU of which ~70% found the
object present: every object whole and legal after every crash, and the namespace mounted every
time. No defect found; the journal's host proof holds on the real device path.

## Consequences

**Good.** Crash consistency is now proved end to end on every CPU, not only on the host model.

**Costs.** About 60-80 s per CPU for 12 rounds.

**Not claimed.** QEMU's raw image is not a disk with a volatile cache: a kill cannot lose a write
the device already acknowledged, so this does not test flush-ordering against a caching device.
