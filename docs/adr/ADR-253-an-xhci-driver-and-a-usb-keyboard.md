# ADR-253 — An xHCI driver and a USB keyboard

**Status:** Accepted (2026-10-15)
**Requirements:** REQ-DRV-012 (new)
**Builds on:** ADR-223 (NVMe: the register seam, the clock-bounded wait, the simulated controller
that lies), ADR-225/226 (AHCI on both PCI CPUs), ADR-228 (drivers safe on a machine that is not a
gate), ADR-080 (the virtio keyboard decoder), ADR-043 (DMA gate).

## Context

ADR-223 queued xHCI first among the drivers real machines need: on most current PCs and every
laptop the keyboard is a USB device, and PS/2 emulation by firmware is not something to rely on.
Aletheia had no USB stack at all.

## Decision

* **`kernel_core::xhci`, one shared driver**, generic over the NVMe register seam (`NvmeRegs`:
  32/64-bit access to a BAR) and `VirtioHal` (frames, barrier, clock), so the hosted tests drive the
  same code the kernels run. Layouts follow xHCI 1.2, USB 2.0 chapter 9 and HID 1.11 appendix B.
* **Bring-up reads the capabilities instead of assuming them**: CAPLENGTH, the runtime and doorbell
  offsets (each checked to lie inside the mapped 16 KiB window), MaxSlots/MaxPorts, 32- or 64-byte
  contexts (CSZ), 64-bit addressing (AC64; a structure above 4 GiB on a 32-bit controller is
  refused), and the scratchpad count (pages provided and listed, up to 64; more is refused by
  name). Halt, reset (bounded wait for HCRST and CNR to clear), 4 KiB pages required, DCBAA,
  command ring, one event ring segment, run, and no Host System or Controller Error.
* **One frame per structure, every frame registered with the DMA gate** before its address
  reaches the controller: DCBAA (with the scratchpad array in its upper half), command ring, event
  ring (with its ERST entry), and per device an input context, an output context, the EP0 ring, the
  interrupt ring, and a data buffer. Rings are one frame each and wrap through a Link TRB.
* **Enumeration**: reset a connected root port (PORTSC written with only PP and PR, so the RW1C
  enable bit is never cleared by accident), read its speed, Enable Slot, Address Device, read the
  first 8 bytes of the device descriptor and Evaluate Context when a full-speed device's EP0 is
  larger than 8, read the device and whole configuration descriptors, and walk the configuration
  for a HID boot keyboard (class 3/1/1) with an interrupt-IN endpoint. SET_CONFIGURATION,
  SET_PROTOCOL(boot), Configure Endpoint, and one report transfer posted.
* **Every completion is checked against what was asked**: a command completion must name the
  command TRB and carry Success; a transfer event must name a TRB of the transfer in flight and the
  device's slot; a descriptor whose lengths do not tile the buffer is refused, not read past.
  Port Status Change events met while waiting are counted and consumed.
* **Keystrokes reuse the virtio keyboard's decoder.** `BootKeyboard` diffs each 8-byte boot report
  against the last and turns every key or modifier that went down or up into an evdev key event
  for `vinput::KeyDecoder` (ADR-080), through the HID usage table Linux uses. A USB keyboard
  therefore types exactly the bytes any other keyboard types; a phantom (ErrorRollOver) report is
  refused and counted.
* **ADR-228 holds**: no controller, nothing connected, or no keyboard among the connected devices
  (a mouse, a hub, a disk) is a named skip, not a failure. Each connected port is tried in turn.
  The suite ends by halting and resetting the controller, and dropping a controller halts it, so
  nothing the kernel told it about is touched afterwards (on x86-64 the VT-d suite runs later and
  still records nothing).
* Polled, like NVMe and AHCI.

## Evidence

* Boot: new family `xhci`, 7 invariants, on aarch64 (QEMU virt, PCIe 00:05.0) and x86-64 (q35,
  00:06.0), each gate now attaching `-device qemu-xhci -device usb-kbd`: controller up, DMA gate
  live, port reset and device addressed, descriptors read and a boot keyboard found, configured
  with its endpoint Running, an idle keyboard types nothing in 20 ms, and the controller halts.
  Suite time 23-24 ms. VT-d 14/14 and SMMU 10/10 unchanged. riscv64 has no PCI bus and gets no
  family, as with NVMe and AHCI.
* Host (`kernel-core/tests/xhci.rs`, 14 tests): a simulated controller with a keyboard on port 1
  runs the driver unchanged. Typed reports (shift+a, b, b held while 1 goes down, Enter) arrive as
  `Ab1\r`; no keystroke, no report; rings lap through their Link TRBs; 64-byte contexts; three
  scratchpad pages provided and registered; a full-speed device with a 64-byte EP0 is evaluated; a
  controller that never leaves reset, runs 8 KiB pages, wants 65 scratchpads or places registers
  outside the window is refused at init; a completion naming the wrong TRB, a failed command, a
  transfer for another slot, a short device descriptor, an output context that never says
  Addressed, and a stalled SET_PROTOCOL are each refused by name; a mouse is not a keyboard, and a
  machine whose only USB device is a mouse skips the suite.

## What this is not

* **Not yet the console's keyboard.** The suite proves the device and the decoder; it then halts
  the controller. Keeping it live and harvesting reports from the console and desktop loops, with
  a QMP `sendkey` e2e as proof, is the next rung. The console's idle wait quiesces the PIT on
  x86-64 (ADR-168), so a polled USB keyboard needs the desktop pump or a timer to be read.
* One device bound per controller, no hubs (a keyboard behind a hub is not found), no USB storage
  or mice, no interrupts or MSI-X, no Reset Endpoint recovery after a stall (the device is refused
  instead).
* No IOMMU window: the controller is halted before VT-d enforcement (x86-64) and is a CLI-plugged
  device the SMMU suite does not translate (aarch64). A live controller after enforcement needs its
  grants in those tables (`Xhci::dma_grants`).
* Never run on physical USB hardware. QEMU's model is the only controller it has met.
