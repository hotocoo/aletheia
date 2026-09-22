# ADR-153 — An entropy source: the bytes a key must be made of

- **Status:** accepted
- **Date:** 2026-09-22
- **Requirement:** REQ-SEC-TLS-011 (closes the gap ADR-151 named)
- **Supersedes:** the timer-seeded ephemeral key of ADR-151.

## Context

ADR-151 shipped a TLS client and said, in its own ADR, STATUS and the Lethe table, that its
ephemeral X25519 key was seeded from timer readings because this kernel had no entropy device — a
key a patient observer could make too, adequate for a gate on a private network and for nothing
else. That was the last named gap in Lethe's stage N2, and a client whose key is predictable is not
a client whose sessions are private.

## Decision

`kernel-core/src/entropy.rs` — a virtio-rng driver on the shared virtqueue substrate (ADR-041's
`virtq.rs`), behind a contract one sentence long: `fill` either fills the whole buffer with bytes
the device produced, or refuses by name.

- **Every draw is checked before it is believed.** A failed random-number device does not
  announce itself; it returns one value, or the last answer again, or nothing. So a draw whose
  bytes are all equal is `Degenerate`, a draw repeating the previous one is `Repeated`, a device
  that does not answer is `Device(...)`, and none of them reaches a key. These are not statistics —
  a statistical test on sixty-four bytes proves nothing — they are the failure modes a broken device
  actually has.
- **The absence has a type.** `NoEntropy` refuses with `Absent`. A console on a machine without the
  device opens no TLS conversation and says so: `this machine has no entropy device; a TLS key from
  a predictable seed is refused`. There is no fallback to the clock; a fallback is how ADR-151's
  caveat would have quietly become permanent.
- **The seed is the device's, once per conversation.** `tls_seed` draws sixty-four fresh bytes;
  `ephemeral_material` separates them into the scalar and the random. The boot suite proves the
  device and then KEEPS it (`netstatic::keep_entropy`), the same shape as the network device
  (ADR-140), so the console's keys come from the device the invariants were proved against.
- **The DMA gate holds here too** (REQ-DRV-006): the driver's one buffer frame is registered before
  the device is told about it, and an address never registered is refused before it becomes a
  descriptor.

Every gate that boots a networked guest now attaches `virtio-rng` (`-device virtio-rng-device` on
the device-tree targets, `virtio-rng-pci` on x86-64); VirtualBox has none, and the second-hypervisor
gate lists the family as skipped by name rather than absent by accident. The VMware package boots
without one and its console says so.

## Proof

`entropy=6` on all three CPUs, each against its own device (virtio-mmio on aarch64 and RISC-V,
PCI on x86-64): a 64-byte request is filled completely; two consecutive draws differ and neither
was refused; a page-sized draw shows at least 200 of the 256 byte values (for uniform bytes the
chance of fewer is under one in a hundred thousand, for a stuck or counting device it is certain);
the DMA gate refuses an unregistered address; no device is a named refusal that seeds no key; two
seeds derive two different ephemeral scalars, neither zero. Host: a stuck source seeds nothing, a
changing one seeds keys that change. Conformance contract 341 -> 347 core behaviours.

The live TLS gate (`scripts/tls-e2e.sh`) runs with the device attached, so the conversation it
proves against OpenSSL is now made with a key from it.

## Alternatives considered

**`RDRAND` / `RNDR` / RISC-V `Zkr`.** Not this rung: `-cpu qemu64` exposes no `RDRAND`, Cortex-A72
has no `FEAT_RNG`, and QEMU's `rv64` needs an extension flag for `seed`. One device every target and
every gate has alike is what makes the contract provable on all three; CPU instructions can join
as further sources later, mixed rather than trusted alone.

**Mixing timer readings in as well.** Rejected: mixing a weak source into a strong one adds
nothing the strong one lacks, and it makes "where did this key come from" a longer answer.

## Consequences

Lethe's stage N2 is delivered without a caveat. `tls` on a machine with the device is a client
whose sessions are private to the extent TLS 1.3 with a pinned root makes them; on a machine
without it, `tls` refuses.
