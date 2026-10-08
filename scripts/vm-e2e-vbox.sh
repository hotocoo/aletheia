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
# VBOX_REQUIRED=1 (set by CI) turns every SKIP-because-VirtualBox-cannot-run into exit 1.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
X86="$ROOT/kernel-x86_64"
BUILD="$X86/build"
IMG="$BUILD/aletheia-x86_64.img"
VDI="$BUILD/aletheia-vbox.vdi"
SVDI="$BUILD/aletheia-vbox-sata-scratch.vdi"
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
  if [ "${VBOX_REQUIRED:-0}" = 1 ]; then echo "VM-E2E-VBOX: FAIL (VBOX_REQUIRED=1 and VirtualBox absent)"; exit 1; fi
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
    if [ "${VBOX_REQUIRED:-0}" = 1 ]; then echo "VM-E2E-VBOX: FAIL (VBOX_REQUIRED=1 and host cannot virtualize x86-64)"; exit 1; fi
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
  'ALL 13 INPUT-ROUTING INVARIANTS HOLD'
  'ALL 7 TEXT-GRID INVARIANTS HOLD'
  'ALL 14 WINDOW-MANAGER INVARIANTS HOLD'
  'ALL 6 WINDOW-STORM INVARIANTS HOLD'
  'ALL 5 SCHEDULER-STORM INVARIANTS HOLD'
  'ALL 5 FILESYSTEM-STORM INVARIANTS HOLD'
  'ALL 5 CONSOLE-STORM INVARIANTS HOLD'
  # Lethe (REQ-ML-007, ADR-165) advises the same contract on every CPU.
  'ALL 12 LETHE ADVISOR INVARIANTS HOLD'
  # The resident governor (REQ-PM-002, ADR-166) stands the watch on every CPU: the tick contract
  # depends on no device, so a second hypervisor must prove it too.
  'ALL 15 RESIDENT GOVERNOR INVARIANTS HOLD'
  'DMA-BOUNDARY INVARIANTS HOLD'
  'INPUT-RING INVARIANTS HOLD'
  'CONSOLE INVARIANTS HOLD'
  # The real-device-class drivers (ADR-224, ADR-225) against VirtualBox's OWN controller models -
  # a second implementation of each, written by someone other than QEMU. (NVMe is absent here:
  # VirtualBox's NVMe controller ships only in the Oracle Extension Pack - VERR_PDM_DEVICE_NOT_FOUND.)
  'ALL 6 E1000 INVARIANTS HOLD'
  'ALL 25 AHCI INVARIANTS HOLD'
  'e2e\] PASS'
)
# SKIPPED-BY-HYPERVISOR: VirtualBox emulates no virtio-blk and this VM has no NIC, so the storage and
# network families cannot run here. They remain REQUIRED on the QEMU gate, which is the only place
# they are proved. Listed so the summary states what this rung did not cover.
SKIPPED=(
  'VIRTIO-BLK INVARIANTS HOLD   (VirtualBox emulates no virtio-blk device)'
  'DURABLE-STORE INVARIANTS HOLD   (needs the virtio-blk scratch disk)'
  'PERSISTENT MEDIUM cross-reboot proof   (needs the virtio-blk persistent disk)'
  'NETWORK INVARIANTS HOLD   (the VM NIC is an e1000, not virtio-net; the e1000 family runs instead)'
  'VIRTIO-GPU INVARIANTS HOLD   (VirtualBox emulates no virtio-gpu device)'
  'FRAMEBUFFER-CONSOLE INVARIANTS HOLD   (needs the virtio-gpu device)'
  'INPUT-HARDWARE INVARIANTS HOLD   (VirtualBox emulates no virtio-input device)'
  'CUSTODY-DELIVERY INVARIANTS HOLD   (needs a persistent virtio-blk disk AND the QEMU fw_cfg channel)'
  'VT-D INVARIANTS HOLD   (VirtualBox declares no DMAR table - the kernel skips the suite green and says why)'
  'ENTROPY INVARIANTS HOLD   (VirtualBox emulates no virtio-rng device - the kernel says so and opens no TLS conversation)'
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

# Every VBoxManage call is bounded: a host-side command that blocks (a poweroff the VM never
# acknowledges, a wedged service) must fail this gate with its name, not hang the job for an hour.
if command -v timeout >/dev/null 2>&1; then
  vbm() { timeout --kill-after=10 "${VBM_CALL_TIMEOUT_S:-90}" "$VBM" "$@"; local rc=$?; [ $rc -eq 124 ] && echo "VBoxManage $1 timed out" >&2; return $rc; }
else
  vbm() { "$VBM" "$@"; }
fi

# --- one full boot at a given guest memory size ---------------------------------------------------
cleanup() {
  # stdout only: a VBoxManage call that times out says so on stderr, into the job log.
  vbm controlvm "$VM_NAME" poweroff >/dev/null
  vbm unregistervm "$VM_NAME" --delete >/dev/null
  vbm closemedium disk "$(hostpath "$VDI")" --delete >/dev/null
  rm -f "$VDI"
  vbm closemedium disk "$(hostpath "$SVDI")" --delete >/dev/null
  rm -f "$SVDI" "$BUILD/scratch-raw.img"
}

# Writes "pass"/"fail"/"" (watchdog) to stdout; everything human-facing goes to stderr so the caller
# can capture the verdict without parsing the transcript.
boot_once() {
  local mem="$1"
  echo "==> [1/4] tearing down any existing '$VM_NAME'" >&2
  cleanup
  rm -f "$LOG"

  echo "==> [2/4] converting raw image -> VDI" >&2
  vbm convertfromraw "$(hostpath "$IMG")" "$(hostpath "$VDI")" --format VDI >/dev/null 2>&1 \
    || { echo "FAIL: convertfromraw" >&2; return 2; }
  # SATA scratch disk (ADR-225): 1 MiB = 256 blocks, the geometry the AHCI suite expects.
  dd if=/dev/zero of="$BUILD/scratch-raw.img" bs=1048576 count=1 2>/dev/null &&
    vbm convertfromraw "$(hostpath "$BUILD/scratch-raw.img")" "$(hostpath "$SVDI")" --format VDI >/dev/null 2>&1 \
    || { echo "FAIL: sata scratch convertfromraw" >&2; return 2; }

  echo "==> [3/4] provisioning VM (EFI firmware, SATA/AHCI, ${CPUS} vCPU, ${mem} MiB, serial -> file)" >&2
  {
    vbm createvm --name "$VM_NAME" --ostype Other_64 --register &&
    # --firmware efi is not optional: VirtualBox defaults to legacy BIOS, which never loads
    # \EFI\BOOT\BOOTX64.EFI and would present as a silent hang rather than a configuration error.
    vbm modifyvm "$VM_NAME" --firmware efi --memory "$mem" --cpus "$CPUS" \
        --graphicscontroller vmsvga --nic1 nat --nictype1 82540EM --audio-driver none &&
    vbm storagectl "$VM_NAME" --name SATA --add sata --controller IntelAhci --portcount 2 --bootable on &&
    vbm storageattach "$VM_NAME" --storagectl SATA --port 0 --device 0 --type hdd \
        --medium "$(hostpath "$VDI")" &&
    # Port 1: the AHCI scratch disk (ADR-225), the only disk the driver may write - marked by serial.
    vbm storageattach "$VM_NAME" --storagectl SATA --port 1 --device 0 --type hdd \
        --medium "$(hostpath "$SVDI")" &&
    vbm setextradata "$VM_NAME" "VBoxInternal/Devices/ahci/0/Config/Port1/SerialNumber" "ALETHEIA-SCRATCH" &&
    # COM1 at the architectural 0x3F8/IRQ4, backed by a host file the gate greps.
    vbm modifyvm "$VM_NAME" --uart1 0x3F8 4 --uart-mode1 file "$(hostpath "$LOG")"
  } >/dev/null 2>&1 || { echo "FAIL: VM provisioning" >&2; return 2; }

  echo "==> [4/4] booting headless (watchdog ${TIMEOUT_S}s)" >&2
  vbm startvm "$VM_NAME" --type headless >&2 \
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
  vbm controlvm "$VM_NAME" poweroff >/dev/null 2>&1
  sleep 1
  printf '%s' "$v"
}

overall=0
for mem in $MEM_SIZES; do
  echo
  echo "======================================================================"
  echo "  BOOT @ ${mem} MiB guest RAM"
  echo "======================================================================"
  # The verdict comes back through a file, not `$(...)`: a VirtualBox service the first call
  # spawns inherits a captured stdout, and the shell would wait for it to close - forever.
  boot_once "$mem" > "$BUILD/vbox-verdict" </dev/null
  verdict="$(cat "$BUILD/vbox-verdict")"
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
