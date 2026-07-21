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
  ->  re-prove 11 capability-secure spine invariants in kernel space  ->  [e2e] PASS
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

## Scope (first bootable milestone)

Delivered: UEFI boot + framebuffer/serial console + GDT/IDT + PIC/PIT interrupt & timer + memory-map
parse + 8 MiB heap + the 11 kernel-space spine invariants. **Deferred (P5):** own page tables /
higher-half, TSS+IST double-fault stack, APIC/HPET + calibrated TSC, SMP, a real page-frame
allocator, and the RISC-V first-class backend. See `../docs/adr/ADR-019-hal-amd64-riscv-targets.md`.
