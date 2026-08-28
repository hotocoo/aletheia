#!/usr/bin/env bash
# End-to-end boot gate for the Aletheia x86-64 microkernel on a SECOND, INDEPENDENT hypervisor:
# Oracle VirtualBox (REQ-QUAL-004, ADR-046).
#
# Every other boot gate in this repository runs on QEMU. A kernel that boots only on QEMU has proved
# "correct against QEMU" — the emulator and the kernel can be wrong together, and no additional QEMU
# testing can find it. VirtualBox disagrees with QEMU in exactly the places that matter: its own EFI
# implementation (not OVMF), its own ACPI tables, SATA/AHCI instead of virtio-blk, and NO
# `isa-debug-exit` device, so the exit-code contract the QEMU gate is built on does not exist here.
#
# The verdict therefore comes from the serial log, and the marker list is shared with the QEMU gate
# so the two rungs cannot drift apart. Capabilities VirtualBox does not emulate are named explicitly
# as SKIPPED and re-named in the summary — never silently absent.
#
# Runs on Linux, macOS and Windows (Git Bash / MSYS): the image is built by the dependency-free
# kernel-x86_64/scripts/mkesp.py, so this gate needs no mtools, no hdiutil, and no QEMU.
#
# Exit 0 = PASS. Exit 0 with "SKIP" = VirtualBox is not installed (never a silent pass). Exit 1 = FAIL.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
X86="$ROOT/kernel-x86_64"
BUILD="$X86/build"
IMG="$BUILD/aletheia-x86_64.img"
VDI="$BUILD/aletheia-vbox.vdi"
LOG="$BUILD/aletheia-vbox-serial.log"
VM_NAME="${VM_NAME:-Aletheia-x86_64-e2e}"
CPUS="${CPUS:-2}"
TIMEOUT_S="${TIMEOUT_S:-180}"

# TWO guest memory sizes, and this is not padding. The firmware's memory map is an INPUT to the
# kernel: it decides where the image is loaded and where the largest conventional region starts, and
# both of those have already hidden real defects behind a single fixed size —
#   * 512 MiB: image at ~0x1c70_0000, pool base at 0x0010_0000 (inside the split first 2 MiB block);
#   * 1 GiB:   image at ~0x3c6c_8000, close enough to the user region that a mis-declared
#              kernel-image extent finally overlapped it (invariant 70).
# Varying the size is the cheapest way to stop the gate from proving "correct against one memory map".
# Set MEM_MB to pin a single size (a developer bisecting one failure); unset runs both.
MEM_SIZES="${MEM_MB:-512 1024}"

# Honor the per-crate nightly toolchain via the rustup shim (a system cargo earlier in PATH ignores
# rust-toolchain.toml and fails cross-compilation with E0463).
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

# --- locate VBoxManage -------------------------------------------------------------------------
VBM=""
for c in "${VBOXMANAGE:-}" "$(command -v VBoxManage 2>/dev/null)" \
         "/c/Program Files/Oracle/VirtualBox/VBoxManage.exe" \
         "/mnt/c/Program Files/Oracle/VirtualBox/VBoxManage.exe" \
         "/usr/lib/virtualbox/VBoxManage" "/usr/bin/VBoxManage" \
         "/Applications/VirtualBox.app/Contents/MacOS/VBoxManage"; do
  [ -n "$c" ] && [ -x "$c" ] && { VBM="$c"; break; }
done
if [ -z "$VBM" ]; then
  echo "SKIP: VBoxManage not found (install Oracle VirtualBox, or set VBOXMANAGE=/path/to/VBoxManage)"
  echo "VM-E2E-VBOX: SKIP (VirtualBox absent — this rung did NOT run)"
  exit 0
fi

# VirtualBox present is not the same as VirtualBox able. This gate boots an x86-64 guest, and
# VirtualBox virtualizes the HOST architecture -- it is not an emulator. On an arm64 host the ARM
# build installs, `VBoxManage --version` answers, and `startvm` then dies at the point where it would
# have needed x86 hardware that is not there.
#
# That used to be reported as FAIL, which is the wrong word for it: nothing about Aletheia was tested
# and nothing about Aletheia was wrong. It is the same situation as a host with no OVMF or no Docker,
# and it gets the same treatment everywhere else in this repository -- SKIP, loudly, naming what did
# not run, so a summary can never read as though a second hypervisor qualified the image when no
# second hypervisor was capable of trying.
HOST_ARCH="$(uname -m)"
case "$HOST_ARCH" in
  x86_64 | amd64) ;;
  *)
    echo "SKIP: this host is $HOST_ARCH and VirtualBox virtualizes the host architecture — it cannot"
    echo "      run an x86-64 guest here. Run this rung on an x86-64 host (see docs/VIRTUALBOX.md)."
    echo "VM-E2E-VBOX: SKIP (host cannot virtualize x86-64 — this rung did NOT run)"
    exit 0
    ;;
esac
echo "==> VBoxManage: $VBM ($("$VBM" --version 2>/dev/null | tr -d '\r'))"

# VBoxManage is a Windows binary under Git Bash/WSL and does not understand POSIX paths.
hostpath() {
  if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"
  elif command -v wslpath >/dev/null 2>&1 && [[ "$VBM" == /mnt/c/* ]]; then wslpath -w "$1"
  else printf '%s' "$1"; fi
}

# --- the markers ------------------------------------------------------------------------------
# REQUIRED: every invariant family that does not depend on a device VirtualBox lacks. Kept as a
# LIST, not as a chain of greps, so a family cannot be dropped by deleting one line unnoticed.
REQUIRED=(
  'ALL 22 MEMORY INVARIANTS HOLD'
  'ALL 14 CAPABILITY-LIFETIME INVARIANTS HOLD'
  'VIRTUAL-MEMORY INVARIANTS HOLD'
  'kernel map built @'
  'kernel map ACTIVE'
  'live W\^X audit: .* 0 violations'
  'SMP INVARIANTS HOLD'
  'RING-3 BOUNDARY INVARIANTS HOLD'
  # Parentheses are ERE groups — escaped, or this matches a line the kernel never prints.
  'TERMINATED \(Fault\(UserNotMapped\)\); system continues'
  'FILESYSTEM INVARIANTS HOLD'
  # The self-benchmark (ALET-P2-010, ADR-064) needs no device - it must hold on VirtualBox too.
  'ALL 12 BENCHMARK INVARIANTS HOLD'
  # The power/performance contract (ALET-P2-022, ADR-076) is an arch-independent model - it
  # must hold wherever the kernel boots, hypervisor or not.
  'ALL 14 POWER-PERFORMANCE INVARIANTS HOLD'
  # The composition contract (ALET-P2-021, ADR-077) is an arch-independent model too.
  'ALL 14 COMPOSITION-CONTRACT INVARIANTS HOLD'
  'DMA-BOUNDARY INVARIANTS HOLD'
  'INPUT-RING INVARIANTS HOLD'
  'CONSOLE INVARIANTS HOLD'
  'e2e\] PASS'
)
# SKIPPED-BY-HYPERVISOR: VirtualBox emulates no virtio-blk and this VM has no NIC, so the storage and
# network families cannot run here. They remain REQUIRED on the QEMU gate, which is the only place
# they are proved. Listed so the summary states what this rung did not cover.
SKIPPED=(
  'VIRTIO-BLK INVARIANTS HOLD   (VirtualBox emulates no virtio-blk device)'
  'DURABLE-STORE INVARIANTS HOLD   (needs the virtio-blk scratch disk)'
  'PERSISTENT MEDIUM cross-reboot proof   (needs the virtio-blk persistent disk)'
  'NETWORK INVARIANTS HOLD   (this VM is provisioned with no NIC)'
  'VIRTIO-GPU INVARIANTS HOLD   (VirtualBox emulates no virtio-gpu device)'
  'FRAMEBUFFER-CONSOLE INVARIANTS HOLD   (needs the virtio-gpu device)'
  'CUSTODY-DELIVERY INVARIANTS HOLD   (needs a persistent virtio-blk disk AND the QEMU fw_cfg channel)'
  'VT-D INVARIANTS HOLD   (VirtualBox declares no DMAR table - the kernel skips the suite green and says why)'
)

# --- build ------------------------------------------------------------------------------------
echo "==> building x86-64 .efi from HEAD (dropping stale artifact)"
EFI="$X86/target/x86_64-unknown-uefi/release/aletheia-kernel-x86_64.efi"
rm -f "$EFI"
( cd "$X86" && cargo build --release ) || { echo "FAIL: build"; echo "VM-E2E-VBOX: FAIL"; exit 1; }

echo "==> assembling GPT/ESP disk image (dependency-free mkesp.py)"
PY="$(command -v python3 || command -v python)"
[ -n "$PY" ] || { echo "FAIL: python3 not found"; echo "VM-E2E-VBOX: FAIL"; exit 1; }
"$PY" "$X86/scripts/mkesp.py" --efi "$EFI" --out "$IMG" \
  || { echo "FAIL: image build"; echo "VM-E2E-VBOX: FAIL"; exit 1; }

# --- one full boot at a given guest memory size ---------------------------------------------------
cleanup() {
  "$VBM" controlvm "$VM_NAME" poweroff >/dev/null 2>&1
  "$VBM" unregistervm "$VM_NAME" --delete >/dev/null 2>&1
  "$VBM" closemedium disk "$(hostpath "$VDI")" --delete >/dev/null 2>&1
  rm -f "$VDI"
}

# Echoes "pass"/"fail"/"" (watchdog) on stdout; everything human-facing goes to stderr so the caller
# can capture the verdict without parsing the transcript.
boot_once() {
  local mem="$1"
  echo "==> [1/4] tearing down any existing '$VM_NAME'" >&2
  cleanup
  rm -f "$LOG"

  echo "==> [2/4] converting raw image -> VDI" >&2
  "$VBM" convertfromraw "$(hostpath "$IMG")" "$(hostpath "$VDI")" --format VDI >/dev/null 2>&1 \
    || { echo "FAIL: convertfromraw" >&2; return 2; }

  echo "==> [3/4] provisioning VM (EFI firmware, SATA/AHCI, ${CPUS} vCPU, ${mem} MiB, serial -> file)" >&2
  {
    "$VBM" createvm --name "$VM_NAME" --ostype Other_64 --register &&
    # --firmware efi is not optional: VirtualBox defaults to legacy BIOS, which never loads
    # \EFI\BOOT\BOOTX64.EFI and would present as a silent hang rather than a configuration error.
    "$VBM" modifyvm "$VM_NAME" --firmware efi --memory "$mem" --cpus "$CPUS" \
        --graphicscontroller vmsvga --nic1 none --audio-driver none &&
    "$VBM" storagectl "$VM_NAME" --name SATA --add sata --controller IntelAhci --portcount 1 --bootable on &&
    "$VBM" storageattach "$VM_NAME" --storagectl SATA --port 0 --device 0 --type hdd \
        --medium "$(hostpath "$VDI")" &&
    # COM1 at the architectural 0x3F8/IRQ4, backed by a host file the gate greps.
    "$VBM" modifyvm "$VM_NAME" --uart1 0x3F8 4 --uart-mode1 file "$(hostpath "$LOG")"
  } >/dev/null 2>&1 || { echo "FAIL: VM provisioning" >&2; return 2; }

  echo "==> [4/4] booting headless (watchdog ${TIMEOUT_S}s)" >&2
  "$VBM" startvm "$VM_NAME" --type headless >/dev/null 2>&1 \
    || { echo "FAIL: startvm (nested virtualization unavailable?)" >&2; return 2; }

  # The kernel halts rather than exiting (no isa-debug-exit here), so the gate watches the log and
  # stops the machine itself the moment it has a verdict — or when the watchdog fires.
  local v=""
  local i
  for i in $(seq 1 "$TIMEOUT_S"); do
    sleep 1
    [ -f "$LOG" ] || continue
    if grep -q 'e2e\] PASS' "$LOG" 2>/dev/null; then v="pass"; break; fi
    if grep -Eq 'FAILED at|FATAL|KERNEL PANIC' "$LOG" 2>/dev/null; then v="fail"; break; fi
  done
  "$VBM" controlvm "$VM_NAME" poweroff >/dev/null 2>&1
  sleep 1
  printf '%s' "$v"
}

overall=0
for mem in $MEM_SIZES; do
  echo
  echo "======================================================================"
  echo "  BOOT @ ${mem} MiB guest RAM"
  echo "======================================================================"
  verdict="$(boot_once "$mem")"
  rc=$?
  if [ "$rc" -eq 2 ]; then cleanup; echo "VM-E2E-VBOX: FAIL"; exit 1; fi

  echo "==== serial log (VirtualBox, ${mem} MiB) ===="
  if [ -f "$LOG" ]; then tr -d '\r' < "$LOG"; else echo "(no serial output at all)"; fi
  echo "============================================"

  if [ -z "$verdict" ]; then
    echo "FAIL @ ${mem} MiB: watchdog — no PASS and no failure marker within ${TIMEOUT_S}s (hang or no boot)"
    overall=1
    continue
  fi

  # Marker parity. Accepting "[e2e] PASS" alone would pass a kernel that skipped half its suites.
  missing=0
  for m in "${REQUIRED[@]}"; do
    if grep -Eq "$m" "$LOG" 2>/dev/null; then
      printf '  ok    %s\n' "$m"
    else
      printf '  MISS  %s\n' "$m"
      missing=$((missing + 1))
    fi
  done
  for s in "${SKIPPED[@]}"; do printf '  SKIP  %s\n' "$s"; done

  if [ "$verdict" = "pass" ] && [ "$missing" -eq 0 ]; then
    echo "  ---- ${mem} MiB: PASS"
  else
    echo "  ---- ${mem} MiB: FAIL (verdict=$verdict missing_markers=$missing)"
    overall=1
  fi
done

cleanup

if [ "$overall" -eq 0 ]; then
  echo
  echo "Booted at: $MEM_SIZES MiB — the firmware memory map is an INPUT, so one size is one map."
  echo "This rung did NOT cover: ${#SKIPPED[@]} device-dependent families (listed SKIP above)."
  echo "VM-E2E-VBOX: PASS"
  exit 0
fi
echo
echo "VM-E2E-VBOX: FAIL"
exit 1
