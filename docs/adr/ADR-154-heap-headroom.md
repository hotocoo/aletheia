# ADR-154 — Heap headroom: the interactive boot ran out, and now says how much it has

- **Status:** accepted
- **Date:** 2026-09-22
- **Requirement:** REQ-QUAL-007 (the machine under load), REQ-CON-001 (the console)
- **Supersedes:** the 12 MiB heap reservations of ADR-072.

## Context

Two CI gates went red on the runner over three pushes and stayed red after re-runs: the live
desktop on the device-tree targets and the comparative bench, both of which boot the INTERACTIVE
aarch64 kernel — every suite, then the desktop, then the console. Neither log showed a failure;
each showed a console that never printed its prompt. Run locally, the same gate showed why:

    [console] namespace: a RAM disk (no persistent device attached)
    [KERNEL PANIC] ... memory allocation of 536576 bytes failed

The console's scratch RAM disk was the first allocation to find the 12 MiB bump heap (ADR-063:
it never frees) already spent. The storms report the watermark near ten megabytes by the
filesystem storm; the desktop's surfaces take most of the rest; ADR-149's handshake suite built
three eighteen-kilobyte handshakes where one would do, and ADR-151's first draft built a pump per
conversation. The last of those tipped it. The selftest gates never saw it, because they exit
before the console; the runner saw it first because the runner boots the interactive kernel.

## Decision

1. **Allocate once, again.** The handshake suite's three pinned handshakes are one, restarted.
   ADR-151's pump was already one before it landed. The rule ADR-063 states — a long-lived path
   allocates at construction and never per event — is the rule; this ADR is what forgetting it
   costs.
2. **Sixteen megabytes, not twelve**, on all three targets (`HEAP_SIZE` in the two linker scripts,
   the static region on x86-64). The machines have 128 and 256 MiB; the frame allocator gives up
   four. The reservation is headroom, not a fix: a heap that never frees is exhausted by growth,
   and growth is what every wave is.
3. **The margin is printed where it matters.** Every selftest boot ends with
   `[boot] heap: N B used, M B free after every suite`, and every interactive console opens with
   `[console] heap: N B used, M B free`. A gate log now shows the number that was about to reach
   zero, on every run, before it does.

## Proof

The aarch64 live-desktop gate passes locally again and reports its margin; the three boot gates,
conformance and the console gates pass with the new reservation; the handshake suite still holds
its twelve invariants with one handshake. The comparative bench's Aletheia leg reaches its prompt.

## Alternatives considered

**Freeing.** A freeing allocator is a different kernel design decision (ADR-063 chose not to,
and says why); it is not taken in a wave whose subject is a boot that ran out of room.

**Trimming suites out of the interactive build.** Rejected: the interactive disk boots as its own
kernel, runs its invariants, then opens the console — that order is the product.

## Consequences

Four more megabytes of headroom, and a number on every log. The next wave that eats it will be
seen eating it.
