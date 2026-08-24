# ADR-072: The custody anchor crosses the platform boundary

**Status:** Accepted · **Date:** 2026-08-24 · **Closes:** the delivery half of ALET-P1-034 and its
combined-transaction question · **Builds on:** ADR-070 (custody is a lifecycle), ADR-067 (the
supply chain is verified, live, and recorded), ADR-025 (secure boot, phased), ADR-061 (the gate
counts itself)

## Context

ADR-070 built the vault's lifecycle and left exactly two things open, both named in its
non-claims:

* **Secure-boot DELIVERY of the root stays REQ-BOOT-001.** Custody of the ROOT remained
  "whoever calls open" — a root nobody delivers is a root nobody accounts for.
* **The capability image and the entity records it describes still commit as separate
  transactions; deciding whether they must be one remained open.**

Both close here, on real hardware, on all three first-class targets.

## Decision

### Delivery: the root arrives over the firmware's own configuration channel

QEMU's fw_cfg interface is the platform channel this wave is measured against: ioports
0x510/0x511 under q35+OVMF, MMIO windows on both virt machines (aarch64 0x0902_0000,
RISC-V 0x1010_0000 — bases verified against each machine's own device tree; sub-region layout
and endianness verified by live probes and against QEMU's own source). The kernel gains a
minimal transport per target behind one two-method trait, and ONE door in kernel-core:

* `bootroot::deliver` classifies what the platform handed over into NAMED facts:
  **Delivered(exactly 32 bytes)**, **RootNotProvided** (live fw_cfg, no such item),
  **FirmwareAbsent** (no fw_cfg signature at all — VirtualBox, bare metal),
  **Malformed(size)** (any other declared size).
* `bootroot::open_custody` is THE door: nothing else opens a CapVault. Only Delivered
  passes; every other shape is refused BY NAME before any byte is decrypted.
* The root is consumed exactly as ADR-070 demands — only its derived sealing subkey is
  retained — so delivery transfers CUSTODY, never a working key.

Fail-closed rules the live probes forced us to write down:

* The fw_cfg directory has NO reserved word after its count; entries begin immediately at
  offset 4. Reading one shifts every later field by four and silently unmatches every name.
* The control register accepts ONLY a two-byte store whose value arrives BIG-ENDIAN on the
  wire; anything else lands in the wrong region or is rejected silently. Both behaviors were
  reproduced against the emulator before being written down.
* A dead bus reads as 0xFF forever — absence is a VALUE, so absence is always nameable.

The third behavior each target must show is ABSENCE: booted without the item, the machine
prints "PLATFORM ROOT ABSENT (RootNotProvided)", keeps every other subsystem running, and
exits clean. Under VirtualBox there is no fw_cfg at all — FirmwareAbsent, the same posture.
One sealed vault must not kill the machine; it must also never pretend custody happened.

### The combined transaction: DECIDED — two commits, mutually detectable

The capability image and the entity store stay TWO commits: merging the AEAD image into the
entity record would put authority bytes under the record-checksum regime and surrender
independent rotation for no integrity gain. What was missing was MUTUAL DETECTION. ADR-070
pinned the residual precisely: rolling BOTH vault objects back to a consistent older pair was
undetectable without an external anchor.

The anchor is the entity store itself. Every PAIRED commit (`bootroot::commit_pair`) seals
the image first (keystore reserve-commit, then image replace) and THEN writes the entity-store
record carrying the vault's keystore generation INSIDE the durable record, under its trailing
checksum. Every custody open enforces the monotone rule

    witnessed_generation <= keystore_counter

and refuses BY NAME (`RolledBack { remembered, found }`) when the medium remembers newer
authority than the vault can show. Crash positions are safe by ORDER — the witness goes last —
so an interrupted pair always leaves witnessed <= found: forward-safe, never a refusal trap.
The host proof constructs exactly ADR-070's undetectable attack (both vault objects rolled back
together, internally consistent and authentic) and shows the witness catching it, naming both
sides of the disagreement; recovery at the generation the medium ACTUALLY pairs with keeps
authority intact — a lockout pending a forward commit, never a destruction of authority. What
remains undetectable is rolling back ALL durable objects at once, which is strictly stronger
than ADR-070's guarantee and still pinned in the host proofs.

## Proofs

* Host: `kernel-core/tests/bootroot.rs` — the directory walker against lying firmware (counts
  past the data, truncated entries, prefix-name lookalikes, dead buses); every wrong size
  refused before a byte is wanted; redelivery byte-stability; the full fourteen-invariant suite
  re-proved over a modeled medium INCLUDING the second boot; the constructed pair-rollback;
  and a crash-position sweep through the paired commit proving witnessed NEVER ahead of found.
* In-kernel: `bootroot::boot_suite`, 14 invariants on every boot of all three targets
  (`[vault] ALL 14 CUSTODY-DELIVERY INVARIANTS HOLD`), run against the REAL firmware channel
  and the REAL persistent medium — including the cross-reboot reopen (gate boots #1 and #2)
  and the tampered-keystore whole-object refusal. Small by ADR-063 doctrine: rotation, rekey
  and retirement depth stay host-side where the sweeps are exhaustive.
* Gates: all three QEMU gates provision the deterministic anchor via -fw_cfg for boots #1/#2
  and DROP it for a THIRD boot that must print the named absence refusal and still reach e2e
  PASS; the marker maps gain vault=14 deliberately (ADR-061). VirtualBox lists the family as
  skipped-by-hypervisor with the reason.
* Conformance: four custody-delivery behaviors join the core contract, worded identically
  across targets because refusals are the security boundary.

## Named non-claims

* fw_cfg delivery is a TRUSTED platform channel, not a MEASURED one: whoever controls the
  platform controls the root, exactly as whoever controls firmware always could. Attestation /
  measured boot / TPM anchoring remain REQ-BOOT-001 Phase 2/3, deferred until hardware.
* The delivered root is a DEMO anchor in the gates (fixed ASCII bytes): secrecy of these bytes
  is not a claim; the MECHANISM of delivery, refusal, and fail-closed absence is.
* Rolling back every durable object simultaneously remains undetectable (pinned in tests).
* The entity-store witness preserves generations across witness writes but does not attempt
  distributed consensus between subsystems; the monotone rule is enforced only at custody-open
  time, in boot order, single-core.
