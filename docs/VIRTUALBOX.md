# Running Aletheia in Oracle VirtualBox

**As of:** 2026-08-09 · ADR-046 (second-hypervisor qualification) · REQ-QUAL-004

This is the shortest path from a fresh clone to *Aletheia running as its own operating system on your
machine*, and to reading the verdict yourself rather than taking this repository's word for it.

Everything here works on **Windows, macOS and Linux**. No QEMU, no OVMF, no `mtools`, no WSL.

---

## 0. What you need

| | |
|---|---|
| **Oracle VirtualBox** | 7.x. Verify: `VBoxManage --version` |
| **Rust** | `rustup` — the pinned toolchain installs itself from `rust-toolchain.toml` |
| **Python 3** | any 3.8+; only the standard library is used |
| **A bash** | Git Bash on Windows, or any shell on macOS/Linux |

If `VBoxManage` is not on your `PATH` (normal on Windows), the gate finds it at
`C:\Program Files\Oracle\VirtualBox\VBoxManage.exe` automatically. To point at a different install,
set `VBOXMANAGE=/path/to/VBoxManage`.

---

## 1. The one command

```bash
./scripts/vm-e2e-vbox.sh
```

That is the whole thing. It builds the kernel from `HEAD`, assembles a bootable GPT/ESP disk image,
provisions a VirtualBox VM from scratch, boots it headless, reads the serial console, checks every
invariant marker, powers the machine off, and deletes the VM.

**PASS looks like this** (the last lines):

```text
  ok    ALL 22 MEMORY INVARIANTS HOLD
  ok    VIRTUAL-MEMORY INVARIANTS HOLD
  ok    kernel map built @
  ok    kernel map ACTIVE
  ok    live W^X audit: .* 0 violations
  ok    SMP INVARIANTS HOLD
  ok    RING-3 BOUNDARY INVARIANTS HOLD
  ok    TERMINATED (Fault(UserNotMapped)); system continues
  ok    FILESYSTEM INVARIANTS HOLD
  ok    DMA-BOUNDARY INVARIANTS HOLD
  ok    INPUT-RING INVARIANTS HOLD
  ok    CONSOLE INVARIANTS HOLD
  ok    e2e] PASS
  SKIP  VIRTIO-BLK INVARIANTS HOLD   (VirtualBox emulates no virtio-blk device)
  SKIP  DURABLE-STORE INVARIANTS HOLD   (needs the virtio-blk scratch disk)
  SKIP  PERSISTENT MEDIUM cross-reboot proof   (needs the virtio-blk persistent disk)
  SKIP  NETWORK INVARIANTS HOLD   (this VM is provisioned with no NIC)

This rung did NOT cover: 4 device-dependent families (listed SKIP above).
VM-E2E-VBOX: PASS
```

Those four `SKIP` lines are deliberate and are re-stated in the summary. VirtualBox does not emulate
virtio-blk and this VM is provisioned with no NIC, so the storage, durability and network families
**cannot** run on this rung — they are proved by the QEMU gate (`./scripts/vm-e2e-x86.sh`) and
nowhere else. A gate that hid this would be claiming coverage it does not have.

The full serial log is printed above the summary and is also left at
`kernel-x86_64/build/aletheia-vbox-serial.log`.

---

## 2. Watching it boot in the VirtualBox GUI

The gate deletes its VM when it finishes. To keep one and watch it, do the two steps yourself:

```bash
# 1. build the image (this is all the gate does before it provisions)
cd kernel-x86_64
cargo build --release
python scripts/mkesp.py \
  --efi target/x86_64-unknown-uefi/release/aletheia-kernel-x86_64.efi \
  --out build/aletheia-x86_64.img
cd ..
```

Then, on Windows (Git Bash) — on macOS/Linux drop the `.exe` and the `$(pwd -W)`:

```bash
VB="/c/Program Files/Oracle/VirtualBox/VBoxManage.exe"
VM="Aletheia"

"$VB" convertfromraw "$(pwd -W)/kernel-x86_64/build/aletheia-x86_64.img" \
                     "$(pwd -W)/kernel-x86_64/build/aletheia.vdi" --format VDI

"$VB" createvm --name "$VM" --ostype Other_64 --register
"$VB" modifyvm "$VM" --firmware efi --memory 512 --cpus 4 \
      --graphicscontroller vmsvga --nic1 none --audio-driver none
"$VB" storagectl "$VM" --name SATA --add sata --controller IntelAhci --portcount 1 --bootable on
"$VB" storageattach "$VM" --storagectl SATA --port 0 --device 0 --type hdd \
      --medium "$(pwd -W)/kernel-x86_64/build/aletheia.vdi"
"$VB" modifyvm "$VM" --uart1 0x3F8 4 --uart-mode1 file "$(pwd -W)/kernel-x86_64/build/serial.log"

"$VB" startvm "$VM"            # a window opens; drop --type headless to see it
```

**`--firmware efi` is not optional.** VirtualBox defaults to legacy BIOS, which never loads
`\EFI\BOOT\BOOTX64.EFI`; the VM would sit at a blank screen with no error.

In the VM window you get the framebuffer console — Aletheia writes to the GOP framebuffer it took
from the firmware. The *machine-checkable* output is the serial line, so read
`kernel-x86_64/build/serial.log` (it is written live; `tail -f` works on macOS/Linux).

The kernel halts after `[e2e] PASS` rather than powering the machine off — VirtualBox has no
`isa-debug-exit` device, so there is nothing for `exit::exit()` to write to. Close the window or
`VBoxManage controlvm "$VM" poweroff` when you have read enough. Tear down with:

```bash
"$VB" controlvm "$VM" poweroff
"$VB" unregistervm "$VM" --delete
```

---

## 3. Typing at it — the interactive console

The gate builds the non-interactive kernel, which runs every suite and halts. There is also a build
that hands the machine to the serial line and waits for you (REQ-CON-001, ADR-044):

```bash
cd kernel-x86_64
CARGO_FEATURES=interactive cargo build --release --features interactive
python scripts/mkesp.py \
  --efi target/x86_64-unknown-uefi/release/aletheia-kernel-x86_64.efi \
  --out build/aletheia-interactive.img
```

Provision as in §2 but point the VDI at `aletheia-interactive.img`, and give serial port 1 a
**host pipe** instead of a file so you can type into it:

```bash
"$VB" modifyvm "$VM" --uart1 0x3F8 4 --uart-mode1 server \\\\.\\pipe\\aletheia   # Windows
"$VB" modifyvm "$VM" --uart1 0x3F8 4 --uart-mode1 server /tmp/aletheia.pipe      # macOS/Linux
```

**The VM window cannot type at it.** Aletheia draws to the GOP framebuffer it took from the
firmware, so the window shows you the prompt — but the console READS from the UART (REQ-CON-002,
ADR-045) and there is no PS/2 keyboard driver, so keystrokes in the VirtualBox window reach nothing.
This surprises everyone once: the machine looks hung when it is in fact waiting on a line nobody is
sending. (Press **Right Ctrl** to release the keyboard back to the host.)

Attach a terminal to the pipe instead. On Windows either use PuTTY's *Serial* mode pointed at
`\.\pipeletheia`, or run the dependency-free equivalent that ships here — start the VM first,
because the pipe exists only while it runs:

```powershell
VBoxManage startvm "Aletheia"
powershell -ExecutionPolicy Bypass -File scripts/serial-console.ps1
```

Elsewhere, `socat -,raw,echo=0 UNIX-CONNECT:/tmp/aletheia.pipe`. Then use the shell:

```text
aletheia> help
aletheia> write notes hello
aletheia> ls
aletheia> cat notes
aletheia> mem
aletheia> halt
```

Input is interrupt-driven (ADR-045), so the machine is not spinning while it waits for you.
`Ctrl-]` detaches `serial-console.ps1` and leaves the VM running; `halt` stops the machine.

---

## 4. Reading the boot yourself

The serial log is the OS narrating its own bring-up. The lines worth knowing:

| Line | What it means |
|---|---|
| `calling ExitBootServices — Aletheia takes ownership of the machine` | the firmware is out; from here the machine is Aletheia's |
| `kernel map built @ … / kernel map ACTIVE (CR3 = …)` | the kernel built its **own** page tables and switched CR3 to them — it is no longer translating through the firmware's tree |
| `live W^X audit: N leaves, 0 violations` | no page in the live address space is writable *and* executable |
| `ALL 22 MEMORY INVARIANTS HOLD` | the frame allocator, ownership model and erase-on-free |
| `ALL 22 SMP INVARIANTS HOLD` | the other 3 cores were woken through INIT-SIPI-SIPI from the ACPI MADT |
| `ALL 34 RING-3 BOUNDARY INVARIANTS HOLD` | the capability-gated syscall boundary, address-space isolation, preemption, the trap-frame round-trip, and the adversarial `#UD`/`#GP` entries |
| `task N TERMINATED (…); system continues` | a task faulted **on purpose** and the supervisor killed it without taking the system down. This is a pass, not an error |
| `[e2e] PASS` | every suite that ran, passed |

One line you will see under VirtualBox and not under QEMU:

```text
[mm] WARNING: exec protections incomplete (NX=true, SMEP=false) — W^X degraded on this CPU
```

VirtualBox does not expose SMEP to the guest. NX is present, so W^X is still enforced by paging and
the live audit still reports zero violations; what is missing is the hardware's extra refusal to
execute *user* pages in ring 0. The kernel reports the degradation rather than assuming the
protection — which is the behavior you want from an OS that cannot verify a CPU feature.

---

## 5. If something goes wrong

| Symptom | Cause |
|---|---|
| `SKIP: VBoxManage not found` | VirtualBox not installed, or set `VBOXMANAGE=/path/to/VBoxManage` |
| `FAIL: startvm (nested virtualization unavailable?)` | you are inside a VM already; VirtualBox cannot nest here |
| VM window blank, no serial output | `--firmware efi` was omitted, so the VM is in BIOS mode |
| `error: … is not a PE image (no MZ header)` | the `--efi` path points at something other than the UEFI build output |
| `env: 'bash\r': No such file or directory` | a clone made before `.gitattributes` landed; re-clone, or `git add --renormalize .` |

Tunables (environment variables): `VM_NAME`, `MEM_MB` (default 512), `CPUS` (default 4),
`TIMEOUT_S` (default 180), `VBOXMANAGE`.

---

## 6. What this proves, and what it does not

**It proves** that the x86-64 kernel boots and holds its invariants on a hypervisor that is *not*
QEMU — different firmware, different chipset model, different storage stack. That is a real
strengthening: an assumption true of OVMF but not of the architecture now has somewhere to surface,
and one already has (`docs/adr/ADR-046-…` §Consequences).

**It does not prove** the kernel runs on real hardware. VirtualBox is a second emulation of the
platform contract, not the contract. `docs/MATURITY.md` grades every subsystem and states plainly
that nothing here is production-ready; read it before quoting any claim above.
