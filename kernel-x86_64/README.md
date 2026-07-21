# Aletheia — bootable AMD64/x86-64 kernel (UEFI)

The first **bootable Aletheia disk image**: Aletheia boots as its *own* operating system on
x86-64 under UEFI firmware (OVMF in QEMU, native UEFI in VMware), takes ownership of the machine
via `ExitBootServices`, brings up its own segments/interrupts/timer, and re-proves the M1
capability-secure spine invariants **in kernel space**. This is ADR-019's first-class AMD64 target,
executed (not just contracted) — done outside-in and boot-verified, not blind hardware code.

Firmware/UEFI is the **hardware/platform integration layer** (ADR-019). The OS above it is entirely
Aletheia-owned — no Linux/macOS/POSIX host, no third-party OS framework between firmware and kernel.

## Artifacts (produced by `scripts/build-image.sh`)

| File | What |
|------|------|
| `build/aletheia-x86_64.img` | Raw GPT disk, one **EFI System Partition** (FAT32) holding `\EFI\BOOT\BOOTX64.EFI` |
| `build/aletheia-x86_64.vmdk` | Same image as a VMware disk (monolithicSparse) |
| `aletheia-x86_64.vmx` | VMware Fusion/Workstation config (UEFI firmware, attaches the vmdk) |

## Boot flow (what the serial log shows)

```
firmware #[entry]  ->  capture GOP framebuffer  ->  ExitBootServices   (Aletheia takes the machine)
  ->  load own GDT (flat 64-bit)  ->  load IDT (CPU exceptions + IRQ0)
  ->  remap 8259 PIC  ->  program 8254 PIT @100Hz  ->  parse UEFI memory map
  ->  sti + PROVE a timer IRQ fires (tick count > 0)
  ->  seed the physical frame allocator from the UEFI map  ->  prove memory-management invariants
  ->  map/unmap over the live page-table hierarchy  ->  prove virtual-memory invariants
  ->  re-prove 11 capability-secure spine invariants in kernel space
  ->  drop to RING 3 + prove the 10 user-mode invariants (cap-gated syscall, isolation,
      per-process PML4 spaces, cooperative + PIT-preemptive multitasking)  ->  [e2e] PASS
```

Two output channels: **serial (COM1)** is the machine-checkable log the smoke test asserts on;
the **GOP framebuffer** is the human-visible console (what you watch in a VM window).

## Build

```bash
cd kernel-x86_64
./scripts/build-image.sh          # -> build/aletheia-x86_64.{img,vmdk}
```

Toolchain: Rust **nightly** + the `x86_64-unknown-uefi` target (precompiled std, no build-std).
`rustup target add x86_64-unknown-uefi` if the build reports a missing target.

## Run in QEMU (UEFI via OVMF, headless + serial)

```bash
./scripts/smoke-test.sh           # boots build/aletheia-x86_64.img, asserts exit 33 + "[e2e] PASS"
```

Equivalent raw command (OVMF ships with Homebrew QEMU — no download):

```bash
QSHARE=$(brew --prefix qemu)/share/qemu
cp "$QSHARE/edk2-i386-vars.fd" /tmp/vars.fd
qemu-system-x86_64 -machine q35 -m 256 \
  -drive if=pflash,format=raw,unit=0,file="$QSHARE/edk2-x86_64-code.fd",readonly=on \
  -drive if=pflash,format=raw,unit=1,file=/tmp/vars.fd \
  -drive format=raw,file=build/aletheia-x86_64.img \
  -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
  -serial stdio                  # drop `-display none` to watch the framebuffer console
```

Exit code `33` = end-to-end PASS (the kernel's `isa-debug-exit` encodes success `0` as `0x10`;
QEMU reports `(0x10<<1)|1 = 33`). VMware has no such device, so on VMware the kernel simply halts
after printing `[e2e] PASS` to serial + framebuffer.

## Run in VMware (Fusion / Workstation)

1. `./scripts/build-image.sh` to produce `build/aletheia-x86_64.vmdk`.
2. Open `aletheia-x86_64.vmx` (firmware is set to `efi`), or create a VM with **UEFI firmware** and
   attach `build/aletheia-x86_64.vmdk` as the boot disk.
3. Power on. The framebuffer shows the boot log ending in `[e2e] PASS`; serial is written to
   `aletheia-serial.log`.

## Scope

Delivered: UEFI boot + framebuffer/serial console + GDT (with ring-3 segments + TSS) + IDT +
PIC/PIT interrupt & timer + memory-map parse + 8 MiB heap + a UEFI-seeded **physical frame
allocator** + **MMU map/unmap** over the live page-table hierarchy + the 11 kernel-space spine
invariants + **ring-3 (CPL 3) user-mode**: the capability-gated `int 0x80` syscall boundary,
hardware address-space isolation, **per-process PML4 address spaces**, and cooperative **plus
PIT-driven preemptive** multitasking (the x86-64 twin of the aarch64 EL0 suite — the same 10
invariants). ABI note: `x86_64-unknown-uefi` makes `extern "C"` the Microsoft x64 ABI, so the
hand-written trap assembly and its boundary functions are declared `extern "sysv64"`.

**Deferred (P5):** higher-half kernel, TSS+IST double-fault stack, APIC/HPET + calibrated TSC, SMP,
and the RISC-V first-class backend. See `../docs/adr/ADR-019-hal-amd64-riscv-targets.md`.
