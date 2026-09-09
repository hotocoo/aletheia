# ADR-131 — x86-64 virtio-input MSI-X wake path

**Date:** 2026-09-09  
**Status:** Implemented and VM-qualified on QEMU q35

## Decision

The x86-64 live desktop uses PCI MSI-X as the primary virtio-input wake mechanism when the
`input-msix` build feature is enabled. The path has four distinct stages:

1. the PCI MSI-X table is discovered from the device's capability list and mapped only through
   the device-memory admission path;
2. the BSP xAPIC is enabled as the MSI target and vector `0x51` is installed in the IDT;
3. the virtio common configuration maps the event queue to MSI-X table entry 0;
4. the hard IRQ handler records a wake, requests foreground service, and sends LAPIC EOI; it does
   not harvest queues, compose frames, or touch the GUI.

When both live input functions successfully bind, the PIT desktop watchdog is reduced from 1 kHz
to 100 Hz. The watchdog remains as a recovery path rather than the normal input wake source.

The timer-only build remains available with `--features interactive` for A/B qualification.

## Verification

`scripts/vinput-e2e.sh` now accepts `VINPUT_FEATURES` and verifies delivery rather than merely
checking configuration. With `interactive,input-msix`, the live QEMU q35 workflow passed all
existing pointer, focus, keyboard, window, close, terminal-routing and quiet-state checks and
observed **29 MSI-X handler hits / 29 wake samples**. The same workflow without `input-msix`
observed **0 MSI-X hits** and live PIT wake samples, proving the comparison path is real.

The guest reports TSC-based wake telemetry through the `input` command. These are wake-to-pump
measurements under the QEMU TCG qualification environment, not a claim about native silicon
latency; QEMU's host scheduling/emulation affects the observed numbers.

## Security posture

MSI-X table programming refuses missing/cyclic capability chains, invalid BARs, overflowed table
ranges, and device-memory mappings rejected by the VM admission policy. Legacy INTx is disabled
after the MSI-X message is fully programmed. The virtio queue's MSI-X vector is read back from the
device; a device refusal is not treated as success.

## Next

Extend the interrupt domain to per-device/per-CPU vector allocation and Intel VT-d interrupt
remapping before treating MSI-X as the final production interrupt-security posture. Benchmark
native hardware and accelerated virtualization separately from QEMU TCG.
