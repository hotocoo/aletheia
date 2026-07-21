# Booting Aletheia

How to build and boot Aletheia on each supported CPU target, and how to run the
end-to-end VM gates. Aletheia is its own operating system (not a Linux app); every
target boots under a hypervisor/emulator and re-proves the capability-secure spine
invariants in kernel space (ADR-010: contract-honest — the code that is documented is
the code that runs and exits with a machine-checkable verdict).

> **Verdict convention.** aarch64 and RISC-V exit the VM with **process exit code 0**
> on success (a nonzero code names the failing invariant). The x86-64 UEFI image uses
> QEMU's `isa-debug-exit`, which cannot emit 0, so its success code is **33**
> (`(0x10 << 1) | 1`). Every gate script also asserts the `[e2e] PASS` marker in the
> serial log, so a wrong exit code can never masquerade as a pass.

## Prerequisites

- **Rust nightly + `rust-src`** (the bare-metal kernels use `-Zbuild-std`; no precompiled
  `core`/`alloc` ships for `*-none`/`*-uefi` targets). A `rust-toolchain.toml` pins nightly
  per kernel crate, so `cargo` selects it automatically.
  - `rustup toolchain install nightly && rustup component add rust-src --toolchain nightly`
- **QEMU** with the aarch64, x86-64, and riscv64 system emulators:
  - macOS: `brew install qemu` (installs all three + the bundled OVMF firmware).
  - Debian/Ubuntu: `apt install qemu-system-arm qemu-system-x86 qemu-system-misc ovmf`.
- The hosted System Core (`aletheia/`) needs only the **stable** host toolchain.

## One command: the full matrix

```bash
cd Aletheia
./scripts/e2e-all.sh      # aarch64 + RISC-V QEMU gates + x86-64 disk-image smoke test -> one PASS/FAIL
```

Individual hosted + per-target gates:

```bash
cd aletheia && cargo test          # hosted System Core suite (M1 + P2 + policy + AI + search)
./scripts/vm-e2e.sh                # aarch64 microkernel in QEMU (spine + memory + virtual-memory + user-mode)
./scripts/vm-e2e-riscv.sh          # RISC-V/RV64GC first-class target (QEMU virt + OpenSBI, S-mode)
```

---

## aarch64 (bootstrap / dev backend)

Boots on QEMU `virt` at EL1, drops to EL0 user-mode, and re-proves **11 spine + 7 memory
+ 13 virtual-memory + 10 EL0 user-mode** invariants (incl. cap-gated `svc` syscall,
per-process TTBR0 address spaces, and GIC/generic-timer preemption).

```bash
cd kernel
cargo run          # builds the ELF and boots it in QEMU; `cargo run` IS the e2e VM test
# exit code 0 = PASS
```

The QEMU invocation is the `runner` in `kernel/.cargo/config.toml`
(`qemu-system-aarch64 -machine virt,gic-version=2 -cpu cortex-a72 … -semihosting … -kernel`).

---

## RISC-V / RV64GC (first-class target)

QEMU loads **OpenSBI** (M-mode) via `-bios default`, which hands off to the Aletheia
`-kernel` ELF in **S-mode**. The kernel drives the NS16550A UART directly, exercises the
S→M **SBI** boundary, brings up the **Sv39 MMU** (physical frame allocator + identity map
+ dynamic map/unmap), drops to **U-mode** with a capability-gated `ecall` boundary, per-process
`satp` address spaces, SBI-timer preemption, and kernel-mediated IPC, and re-proves the spine
invariants. Machine exit is the **SiFive-test** device (MMIO `0x0010_0000`), which can encode a
failing invariant — SBI SRST cannot.

Gated: **11 spine + 7 memory + 13 virtual-memory + 13 U-mode boundary** invariants (full parity
with the aarch64 and x86-64 user-mode suites).

```bash
cd kernel-riscv64
cargo run          # builds + boots in QEMU riscv64 'virt' + OpenSBI; exit code 0 = PASS
# or the CI gate with a 60s watchdog + marker assertions:
cd .. && ./scripts/vm-e2e-riscv.sh
```

Manual QEMU line (what the gate runs):

```bash
qemu-system-riscv64 -machine virt -cpu rv64 -smp 1 -m 128M -nographic \
  -bios default \
  -kernel kernel-riscv64/target/riscv64gc-unknown-none-elf/debug/aletheia-kernel-riscv64
```

---

## x86-64 / AMD64 (first-class target) — a real bootable disk image

The only target that produces a **bootable disk image**: Aletheia boots as its own OS under
**UEFI firmware**, calls `ExitBootServices` to take the machine, brings up its own GDT/IDT +
8259 PIC + 8254 PIT, and re-proves **11 spine + 7 memory + 6 virtual-memory + 13 ring-3
user-mode** invariants. The image is GPT with a FAT32 **EFI System Partition** holding
`\EFI\BOOT\BOOTX64.EFI` — so it needs **UEFI firmware, never legacy BIOS**.

### Build the image

```bash
cd kernel-x86_64
bash scripts/build-image.sh        # -> build/aletheia-x86_64.img  (raw GPT/ESP)
                                   #    build/aletheia-x86_64.vmdk (VMware)
bash scripts/build-vbox.sh         # -> build/aletheia-x86_64.vdi  (VirtualBox, optional)
```

`build-image.sh` uses only the Rust toolchain + macOS `hdiutil`/`diskutil` + `python3` +
`qemu-img` (no `mtools`/`xorriso`/`grub`). By default it embeds the **release** `.efi`; set
`PROFILE=debug` or `EFI=/path/to.efi` to override, and `SIZE_MB=` to resize.

### Boot it — QEMU + OVMF (the automated gate)

```bash
cd kernel-x86_64
bash scripts/smoke-test.sh         # boots build/aletheia-x86_64.img, asserts exit 33 + [e2e] PASS
```

Manual QEMU line (OVMF **must** be attached as `pflash`, not `-bios`):

```bash
OVMF=/opt/homebrew/share/qemu/edk2-x86_64-code.fd        # macOS/homebrew
# OVMF=/usr/share/OVMF/OVMF_CODE.fd                       # Debian/Ubuntu (ovmf package)
cp /opt/homebrew/share/qemu/edk2-i386-vars.fd /tmp/vars.fd   # writable NVRAM copy

qemu-system-x86_64 -machine q35 -m 256 \
  -drive if=pflash,format=raw,unit=0,file="$OVMF",readonly=on \
  -drive if=pflash,format=raw,unit=1,file=/tmp/vars.fd \
  -drive format=raw,file=build/aletheia-x86_64.img \
  -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
  -serial stdio -display none -no-reboot
# QEMU process exit code 33 = PASS
```

### Boot it — VMware

1. Create a new VM, **Firmware type = UEFI** (not BIOS).
2. Remove the default disk; **attach `build/aletheia-x86_64.vmdk`** as the hard disk.
   (`kernel-x86_64/aletheia-x86_64.vmx` is a ready-made config referencing it.)
3. Optional: add a serial port to a file to capture the `[e2e] PASS` boot log.
4. Power on. The VM boots Aletheia; it halts after the invariant suite.

### Boot it — VirtualBox

1. `bash kernel-x86_64/scripts/build-vbox.sh` to produce `build/aletheia-x86_64.vdi`.
2. New VM, type **Other/Unknown (64-bit)**; in **Settings → System**, enable **EFI**
   (*"Enable EFI (special OSes only)"*).
3. Attach `build/aletheia-x86_64.vdi` as the SATA/IDE hard disk.
4. Optional: **Settings → Serial Ports** → enable, mode *Raw File*, to capture the boot log.
5. Start. Aletheia boots under the VirtualBox EFI firmware.

---

## Troubleshooting

- **"No bootable device" / firmware shell / black screen.** The image is UEFI-only. Ensure
  the VM/emulator uses **UEFI firmware** (OVMF for QEMU, EFI enabled for VMware/VirtualBox),
  never legacy BIOS. On QEMU, OVMF must be attached via `-drive if=pflash,…`, **not** `-bios`.
- **`OVMF firmware not found`.** Point `OVMF` at your platform's OVMF code file
  (`/opt/homebrew/share/qemu/edk2-x86_64-code.fd` on macOS/homebrew;
  `/usr/share/OVMF/OVMF_CODE.fd` on Debian/Ubuntu). The `edk2-i386-vars.fd` NVRAM file must be
  copied to a **writable** location before boot.
- **QEMU "exits with 33" looks like a failure — it isn't.** 33 is the x86-64 image's PASS
  code (`isa-debug-exit` cannot emit 0). aarch64/RISC-V use exit code 0. Always cross-check
  the `[e2e] PASS` line in the serial log.
- **`error: "-Z build-std" … requires rust-src`.** Install it for nightly:
  `rustup component add rust-src --toolchain nightly`.
- **RISC-V hangs / no output.** Confirm QEMU has the riscv64 system target and OpenSBI is
  loaded via `-bios default` (bundled with QEMU ≥ 7). The gate wraps the boot in a 60s
  watchdog so a stall fails loudly instead of hanging.
- **A kernel that traps prints its cause and exits nonzero** (aarch64/RISC-V: an unexpected
  CPU/S-mode trap → exit 102; a panic → 101; a failed invariant → an offset + its index).
  Read the serial log: the last `[pass NN]`/`[FAIL NN]` line names the exact invariant.
- **Rebuild from a clean image.** `rm -rf kernel-x86_64/build` then re-run `build-image.sh`;
  the script does not recompile if the `.efi` already exists, so also `cargo clean` in
  `kernel-x86_64/` if you want a fresh kernel binary.
