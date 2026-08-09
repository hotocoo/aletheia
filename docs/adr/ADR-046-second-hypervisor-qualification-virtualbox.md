# ADR-046 — A second, independent hypervisor: Oracle VirtualBox qualification

**Status:** Accepted (2026-08-09)
**Context:** REQ-QUAL-004 · extends ADR-013 (VM-then-hardware qualification) · informed by
`docs/research/RUST-OS-DEEP-RESEARCH.md` §5.2 · closes the "single-hypervisor" exposure

## Context

Every boot gate in this repository runs on QEMU — `scripts/vm-e2e.sh` (aarch64),
`scripts/vm-e2e-riscv.sh` (RISC-V), `scripts/vm-e2e-x86.sh` (x86-64), and the conformance and console
gates that build on them. ADR-013 says qualification goes *VM, then hardware*. It does not say
**which** VM, and in practice "VM" has meant one implementation of the platform contract.

That is a weaker claim than it reads as. A kernel that boots on QEMU has proved *correct against
QEMU*. QEMU is a careful emulator, but it is also a specific set of choices: OVMF/edk2 firmware, the
q35 chipset model, a virtio-first device story, and an `isa-debug-exit` device that exists nowhere
else. Every one of those is a place where the kernel can acquire an assumption that the architecture
never promised, and no amount of additional QEMU testing can find it — the emulator and the kernel
would be wrong together.

Oracle VirtualBox is the right second rung, precisely because it disagrees with QEMU in the places
that matter:

| | QEMU (existing gate) | VirtualBox (this ADR) |
|---|---|---|
| firmware | OVMF / edk2 (upstream TianoCore build) | VirtualBox's own EFI implementation |
| chipset | q35 (ICH9) | PIIX3 / ICH9 with VirtualBox's own ACPI tables |
| boot storage | raw `-drive`, virtio or AHCI | SATA/AHCI, NVMe, or VirtIO-SCSI — **no virtio-blk** |
| exit signalling | `isa-debug-exit` at port 0xF4 → process exit code | **nothing**; the port write is a no-op |
| serial | `-serial file:` | `--uart-mode1 file` (host-file backend) |
| image format | raw `.img` | VDI (converted from raw) attached to a controller |
| VM lifecycle | a child process with an exit code | a daemon-managed VM queried with `VBoxManage showvminfo` |

Four of those rows are exactly the class of assumption §2.2 of the research document warns about, and
one of them — `isa-debug-exit` — is a QEMU-only device that the entire existing pass/fail contract is
built on.

There is a second, unrelated reason to want this gate: **it is the first gate a developer can run on a
Windows workstation.** The existing x86-64 path needs `qemu-system-x86_64`, `ovmf`, and `mtools`,
which is a Linux/macOS toolchain. VirtualBox runs natively on Windows, macOS, and Linux, so the
qualification story stops being host-shaped.

## Decision

**Add a third qualification rung: the same image, booted by a different hypervisor, judged by the
serial log rather than by a process exit code.**

1. **The image is the same artifact, byte for byte.** The gate does not build a "VirtualBox variant".
   It consumes `kernel-x86_64/build/aletheia-x86_64.img` — the artifact `vm-e2e-x86.sh` already boots
   under QEMU — and converts it to VDI with `VBoxManage convertfromraw`. If the two gates ever
   disagree, the difference is the hypervisor and nothing else. A separate build would have made every
   disagreement ambiguous.

2. **The image builder becomes host-independent, because the gate is.** `build-image.sh` is macOS-only
   (`hdiutil`/`diskutil`) and `build-image-linux.sh` needs `mtools`; neither runs on a Windows host, so
   a VirtualBox gate that depended on them would have inherited the host-shape it exists to remove.
   `kernel-x86_64/scripts/mkesp.py` is a dependency-free Python FAT32/GPT ESP writer that produces the
   same artifact on all three hosts. This is a *precondition* of the decision, not a side quest.

3. **The verdict comes from the serial log, and the log alone.** VirtualBox has no `isa-debug-exit`,
   so `exit::exit()`'s port-0xF4 write is a no-op there and the kernel falls through to its permanent
   `cli; hlt` — which is correct behavior for firmware without the device, and is exactly why that
   fallback was written. The consequence is that **the QEMU pass criterion (process exit 33) does not
   exist on this rung**. The gate therefore:
   - polls the serial log file while the VM runs,
   - passes when it observes `[e2e] PASS` **and every invariant-family marker the QEMU gate requires**,
   - fails on any `FAILED at`/`FATAL` marker, or on the watchdog,
   - and powers the VM off itself in every one of those cases.

   **Marker parity is the whole point.** A gate that accepted `[e2e] PASS` alone would pass a kernel
   that skipped half its suites. The marker list is shared with the QEMU gate rather than retyped, so
   the two rungs cannot drift apart silently.

4. **Absent capabilities SKIP loudly; they never pass quietly.** VirtualBox does not emulate
   virtio-blk. `virtio::selftest()` already returns `Ok(0)` and logs a graceful skip when it finds no
   device, so the storage-dependent markers — virtio-blk, durable store, the persistent-medium
   cross-reboot proof — **cannot** be required on this rung. They are listed explicitly as
   VirtualBox-skipped, printed as `SKIP` in the summary, and named again in the final verdict line.
   The list is a constant in the script, not an implicit consequence of which greps happen to be
   present, so adding a marker to the QEMU gate cannot silently drop it here.

   This is the repository's existing never-a-silent-pass doctrine applied to a hypervisor's device
   model rather than to an unattached disk.

5. **The VM is provisioned from scratch, every run, and torn down after.** `--firmware efi` (VirtualBox
   defaults to BIOS, which would never load `\EFI\BOOT\BOOTX64.EFI`), SATA/AHCI boot disk, no network,
   serial port 1 at 0x3F8/IRQ4 to a host file. Idempotent teardown first, so a crashed previous run
   cannot make the next one boot a stale disk — the same hazard the x86-64 gate handles by deleting a
   stale `.efi`.

6. **The gate is advisory in CI, not blocking — and says so.** GitHub-hosted and GitLab-shared runners
   are themselves virtual machines, and nested virtualization is not available on them, so VirtualBox
   cannot start a VM there. Wiring a job that is guaranteed to fail would train people to ignore a red
   pipeline. The job is therefore declared with an explicit "requires a nested-virtualization-capable
   runner" guard that reports **SKIP** on an incapable runner, and `scripts/check-ci-parity.sh` learns
   about the rung so the two pipelines still have to agree about it.

7. **The gate boots at two guest memory sizes, and that is not padding.** The firmware's memory map
   is an *input* to the kernel — it decides where the image is loaded and where the largest
   conventional region starts — so a gate that always passes the same size proves "correct against
   one memory map". Both defects below were found by varying it. `MEM_MB` pins a single size for a
   developer bisecting one failure; unset runs 512 MiB and 1 GiB.

## What it actually found

Recorded here because an ADR that predicts value and never reports any is a hypothesis, not a
decision. Two real defects, on the first two runs:

1. **Virtual-memory invariant 48 had encoded OVMF's memory map.** It required `frames::base()` to be
   covered by a 2 MiB block. Under OVMF the largest conventional region starts well above 2 MiB, so
   it was; under VirtualBox's EFI it starts at `0x100000` — inside the *first* 2 MiB block, which
   this map deliberately splits to 4 KiB pages so VA 0 can be left with no leaf (ALET-P1-006). The
   invariant had fused a **security** property (bulk RAM is RW+NX) with a **structural** one (it gets
   huge pages). Now two invariants: the security one holds at any granularity, and the structural one
   probes an address proved to lie outside every split span (`kmap::bulk_ram_probe`).

2. **The declared address-space layout was wrong, and had always been wrong.** `vm::layout()` read
   `kmap::image_span()`'s `(base, end)` as `(base, size)` and declared `base .. base + end` — an
   extent roughly twice the image's true length. At 512 MiB the image loads around `0x1c70_0000`, so
   even the doubled extent stopped short of the user region at `0x4000_0000` and `Layout::validate`
   had nothing to complain about. At 1 GiB the image lands at `0x3c6c_8000`, the bogus extent runs to
   `0x7965_d000`, and the declaration **overlaps** the user region — invariant 70 failed. The declared
   layout is what every other layout check is checked *against*, so a wrong declaration is worse than
   a missing one. Neither hypervisor alone would have found this; it took a second hypervisor **and**
   a second memory size.

A third difference is behavioral rather than a defect: **VirtualBox does not expose SMEP**, so the
boot prints `exec protections incomplete (NX=true, SMEP=false) — W^X degraded on this CPU`. NX is
present, the live W^X audit still reports zero violations, and the kernel reports the degradation
instead of assuming a CPU feature it could not verify — which is the behavior the gate should want.

## Consequences

**What this buys.** The x86-64 kernel is now qualified against two independent implementations of the
platform contract. Any assumption the kernel holds that is true of OVMF/q35 but not of the
architecture is now catchable, and the class of bug that "QEMU and the kernel are wrong together"
covers has somewhere to surface. It is also the first x86-64 boot gate that runs on a Windows host.

**What it does not buy, stated plainly.**

* **This is not hardware qualification.** VirtualBox is a second emulation of the contract, not the
  contract. ADR-013's hardware rung is untouched and remains open.
* **The storage stack is less covered here, not more.** No virtio-blk means the journal, the durable
  store, and the cross-reboot "the OS remembers" proof do not run on this rung at all. The QEMU gate
  remains the only place those are proved. Giving VirtualBox a NVMe or VirtIO-SCSI boot path would
  change that, and is named as follow-on work, not claimed.
* **The verdict is weaker in kind.** A serial-log verdict trusts that the kernel reached the code that
  prints the marker; a process exit code additionally proves the machine *stopped* in a controlled
  way. The gate cannot tell "halted after PASS" from "halted after PASS and then a hardware watchdog
  reset", because it powers the VM off itself. Making the kernel power the machine down through ACPI
  S5 would restore the stronger verdict on every hypervisor; that requires FADT/DSDT parsing for
  `\_S5` and is **not** done here.
* **Timing, not logic, is the flaky surface.** The gate polls a file the VirtualBox process is writing.
  It therefore reads the log after teardown as well as during, and treats an empty log at timeout as a
  failure with the log dumped, so a hang is diagnosable rather than mute.
* **A developer without VirtualBox installed gets a SKIP**, printed and re-named in the summary — the
  same treatment an absent `cargo-audit` gets in the quality gate.
