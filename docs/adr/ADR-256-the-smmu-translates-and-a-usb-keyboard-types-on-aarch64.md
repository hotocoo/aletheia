# ADR-256 — The SMMU really translates, and a USB keyboard types on aarch64

**Status:** Accepted (2026-10-15)
**Requirements:** REQ-DRV-015 (new); corrects ADR-074 (ALET-P1-018)
**Builds on:** ADR-255 (USB keyboard at the x86-64 console), ADR-074 (SMMUv3 identity domain),
ADR-253 (xHCI driver), ADR-085 (the ARM generic timer under the console).

## Context

Keeping the USB keyboard on aarch64 the way ADR-255 keeps it on x86-64 failed: the keyboard was
enumerated, configured and polled, yet no key ever arrived. USBSTS showed Host Controller Error
after the first keystroke. The SMMU's own event queue held `C_BAD_STE` (type 0x04) for stream
0x18, the xHCI function, and QEMU (`-d guest_errors`) logged "invalid STE" for every DMA the
controller attempted. The kept keyboard was the first PCI device on aarch64 that had to DMA after
SMMU enforcement latched (ADR-074), and it showed that no PCI device ever could.

Two defects in the ADR-074 code, both in the stream table, neither caught by the host tests
because those decoded with the same constants they encoded with:

1. **STE.Config values were wrong.** `ste_cfg` encoded stage-2 translate as `0b010`. The SMMUv3
   STE.Config field sets bit 2 for every non-abort configuration: abort `0b000`, bypass `0b100`,
   stage-1 `0b101`, stage-2 `0b110`, nested `0b111`; `0b001..0b011` are reserved and make the
   STE illegal. Every STE the kernel wrote was refused.
2. **The linear stream table was not aligned to its size.** The suite declared 256 entries
   (`LOG2SIZE` 8, 16 KiB) but gave the table one 4 KiB frame at a page-aligned address. A linear
   table's base is aligned to the table's size (QEMU masks the low `LOG2SIZE + 6` bits, as the
   architecture allows), so the unit read STEs 4 KiB below where they were written: zeros, V=0.

ALET-P1-018 recorded the consequence as an emulator artifact ("a CLI-attached virtio-blk-pci does
not route DMA through the legacy iommu=smmuv3 unit"). It was not the emulator.

## Decision

* **`ste_cfg` carries the architectural values** (`S2_ONLY = 0b110`, and so on), and a host test
  pins the encoded word0 of a stage-2 STE to `V=1, Config=0b110` rather than decoding it back.
* **The stream table is one frame of 64 entries** (`STRTAB_LOG2 = 6`), which a 4 KiB frame
  holds exactly and is naturally aligned to. `Controller::set_strtab` refuses a base not aligned
  to `2^(LOG2SIZE+6)` (host-tested with a 16 KiB table on a 4 KiB boundary). Requester ids past 63
  (bus 0 devices 8 and up) are refused by name in the suite rather than written outside the frame;
  a larger bus needs a two-level table, which this ADR does not build.
* **aarch64 keeps the USB keyboard** exactly as x86-64 does (ADR-255): `pci::xhci_keep` re-opens
  the controller after its suite, `usbkbd` holds a `KeyboardInput`, `getc` asks it after the UART
  ring, and `conirq::init` arms the EL1 physical timer at 100 Hz when a keyboard is held and no
  desktop is live (with a desktop, the timer runs at the desktop's rate as before). The controller's
  frames come from the allocator pool the stage-2 identity domain covers, so no separate grant is
  needed.

## Evidence

* `scripts/usbkbd-e2e.sh` grows an aarch64 leg (QEMU virt with `iommu=smmuv3`, the DTB over
  fw_cfg, a virtio-blk-pci the SMMU suite needs, `qemu-xhci` + `usb-kbd`; virt has no other
  keyboard, so `input-send-event` can only reach the USB one): `echo Usb` runs, `write`/`cat` with
  an arrow-key edit reads `hello` back. 5/5 on both legs. Before the two fixes the same leg failed
  every typing check with USBSTS.HCE set and the SMMU event queue full of `C_BAD_STE` for SID 0x18.
* `vm-e2e.sh` (aarch64) passes twice with `smmu=10` unchanged; x86-64 and riscv64 gates pass.
* Idle cost of a held USB keyboard on aarch64 (QEMU CPU time over 20 s at the prompt): 0.00 %
  without, 0.70 % with (x86-64: 0.00 / 0.75 %, ADR-255).
* Host: `a_stage_two_ste_carries_the_spec_config_value`; the misaligned-table refusal; 16 SMMU
  host tests.

## What this is not

* The SMMU suite still does not ASSERT a translated completion; now that PCI DMA demonstrably
  works under enforcement (the keyboard is the proof), "a granted function's DMA completes, a
  revoked page faults by name" can become invariants on ARM in a later wave, as ADR-075 did on
  x86-64.
* No per-device windows on ARM: every function shares the identity domain over the frame pool.
* The live desktop's pump does not read the USB keyboard on either CPU.
