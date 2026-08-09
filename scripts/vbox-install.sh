#!/usr/bin/env bash
# Install Aletheia as a PERSISTENT Oracle VirtualBox machine you can start, watch and type at
# (ADR-046, docs/VIRTUALBOX.md). This is the "run it as an OS" path, not the gate.
#
# scripts/vm-e2e-vbox.sh is a GATE: it provisions, boots, judges and then DELETES the VM, because a
# gate that leaves state behind is a gate whose next run boots something it did not build. That is
# the wrong tool for sitting in front of the machine. This script builds the same image and leaves a
# registered VM in place.
#
#   ./scripts/vbox-install.sh                # non-interactive build: runs every suite, halts
#   ./scripts/vbox-install.sh --interactive  # hands the machine to the serial line and waits for you
#   ./scripts/vbox-install.sh --start        # ...and launch it in the GUI when done
#
# Runs on Windows (Git Bash), macOS and Linux. Needs only VirtualBox, Rust and Python 3.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
X86="$ROOT/kernel-x86_64"
BUILD="$X86/build"
VM_NAME="${VM_NAME:-Aletheia}"
# 512 MiB and 2 vCPUs, because that is what this OS actually needs and a default that asks for more
# is a claim about the kernel nobody measured. Both are gated at these values (scripts/vm-e2e-vbox.sh)
# and both are overridable for anyone who wants a bigger machine.
MEM_MB="${MEM_MB:-512}"
CPUS="${CPUS:-2}"

INTERACTIVE=0
START=0
for a in "$@"; do
  case "$a" in
    --interactive) INTERACTIVE=1 ;;
    --start) START=1 ;;
    -h|--help) sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $a (try --help)"; exit 2 ;;
  esac
done

if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

VBM=""
for c in "${VBOXMANAGE:-}" "$(command -v VBoxManage 2>/dev/null)" \
         "/c/Program Files/Oracle/VirtualBox/VBoxManage.exe" \
         "/mnt/c/Program Files/Oracle/VirtualBox/VBoxManage.exe" \
         "/usr/lib/virtualbox/VBoxManage" "/usr/bin/VBoxManage" \
         "/Applications/VirtualBox.app/Contents/MacOS/VBoxManage"; do
  [ -n "$c" ] && [ -x "$c" ] && { VBM="$c"; break; }
done
[ -n "$VBM" ] || { echo "error: VBoxManage not found — install Oracle VirtualBox, or set VBOXMANAGE=/path/to/VBoxManage"; exit 1; }

hostpath() {
  if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"
  elif command -v wslpath >/dev/null 2>&1 && [[ "$VBM" == /mnt/c/* ]]; then wslpath -w "$1"
  else printf '%s' "$1"; fi
}

if [ "$INTERACTIVE" = "1" ]; then
  IMG="$BUILD/aletheia-interactive.img"; VDI="$BUILD/aletheia-interactive.vdi"; FEATURES="--features interactive"
else
  IMG="$BUILD/aletheia-x86_64.img";      VDI="$BUILD/aletheia.vdi";             FEATURES=""
fi
SERIAL="$BUILD/$VM_NAME-serial.log"

echo "==> [1/4] building the kernel (.efi)${INTERACTIVE:+ }$([ "$INTERACTIVE" = 1 ] && echo '— interactive console build')"
EFI="$X86/target/x86_64-unknown-uefi/release/aletheia-kernel-x86_64.efi"
rm -f "$EFI"
# shellcheck disable=SC2086
( cd "$X86" && cargo build --release $FEATURES ) || { echo "FAIL: build"; exit 1; }

echo "==> [2/4] assembling the bootable GPT/ESP disk image"
PY="$(command -v python3 || command -v python)"
[ -n "$PY" ] || { echo "FAIL: python3 not found"; exit 1; }
"$PY" "$X86/scripts/mkesp.py" --efi "$EFI" --out "$IMG" || { echo "FAIL: image"; exit 1; }

echo "==> [3/4] (re)provisioning VirtualBox machine '$VM_NAME'"
# Idempotent: re-running replaces the machine and its disk rather than layering on a stale one.
"$VBM" controlvm "$VM_NAME" poweroff >/dev/null 2>&1
"$VBM" unregistervm "$VM_NAME" --delete >/dev/null 2>&1
"$VBM" closemedium disk "$(hostpath "$VDI")" --delete >/dev/null 2>&1
rm -f "$VDI"
"$VBM" convertfromraw "$(hostpath "$IMG")" "$(hostpath "$VDI")" --format VDI >/dev/null 2>&1 \
  || { echo "FAIL: convertfromraw"; exit 1; }

{
  "$VBM" createvm --name "$VM_NAME" --ostype Other_64 --register &&
  # --firmware efi is mandatory: VirtualBox defaults to legacy BIOS, which never loads
  # \EFI\BOOT\BOOTX64.EFI, and the failure presents as a blank screen rather than an error.
  "$VBM" modifyvm "$VM_NAME" --firmware efi --memory "$MEM_MB" --cpus "$CPUS" \
      --graphicscontroller vmsvga --nic1 none --audio-driver none &&
  "$VBM" storagectl "$VM_NAME" --name SATA --add sata --controller IntelAhci --portcount 1 --bootable on &&
  "$VBM" storageattach "$VM_NAME" --storagectl SATA --port 0 --device 0 --type hdd --medium "$(hostpath "$VDI")"
} >/dev/null 2>&1 || { echo "FAIL: provisioning"; exit 1; }

echo "==> [4/4] wiring serial port 1 (0x3F8 / IRQ 4)"
if [ "$INTERACTIVE" = "1" ]; then
  # A host PIPE, so a terminal can type INTO the machine. A file backend is write-only from the
  # guest's side — fine for the gate, useless for a shell.
  if command -v cygpath >/dev/null 2>&1; then PIPE='\\.\pipe\aletheia'; else PIPE="/tmp/aletheia.pipe"; fi
  "$VBM" modifyvm "$VM_NAME" --uart1 0x3F8 4 --uart-mode1 server "$PIPE" >/dev/null 2>&1
  SERIAL_DESC="host pipe $PIPE"
else
  rm -f "$SERIAL"
  "$VBM" modifyvm "$VM_NAME" --uart1 0x3F8 4 --uart-mode1 file "$(hostpath "$SERIAL")" >/dev/null 2>&1
  SERIAL_DESC="$SERIAL"
fi

cat <<EOF

Installed: VirtualBox machine "$VM_NAME"
  disk    $VDI  (from $IMG)
  memory  $MEM_MB MiB    vCPUs  $CPUS    firmware  EFI
  serial  $SERIAL_DESC

  start (GUI)       VBoxManage startvm "$VM_NAME"
  start (headless)  VBoxManage startvm "$VM_NAME" --type headless
  stop              VBoxManage controlvm "$VM_NAME" poweroff
  remove            VBoxManage unregistervm "$VM_NAME" --delete

EOF
if [ "$INTERACTIVE" = "1" ]; then
  echo "  Attach a terminal to the serial pipe to get the Aletheia shell:"
  echo "    Windows : PuTTY -> Serial -> $PIPE"
  echo "    other   : socat -,raw,echo=0 UNIX-CONNECT:$PIPE"
  echo "    commands: help  ls  write <name> <text>  cat <name>  rm <name>  mem  halt"
else
  echo "  The boot runs every suite and halts. Read the verdict with:"
  echo "    grep -E 'INVARIANTS HOLD|e2e\\] PASS' \"$SERIAL\""
fi
echo "  Full walkthrough: docs/VIRTUALBOX.md"

if [ "$START" = "1" ]; then
  echo
  echo "==> starting '$VM_NAME'"
  "$VBM" startvm "$VM_NAME"
fi
