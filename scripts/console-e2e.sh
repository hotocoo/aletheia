#!/usr/bin/env bash
# Interactive-console end-to-end gate (REQ-CON-001, ADR-044).
#
# Every other gate proves the OS behaves while it BOOTS. This one proves it behaves while somebody
# is USING it: the kernel is built with `--features interactive`, so after the invariant suites it
# hands the machine to the serial line instead of exiting — and then a scripted operator types at
# it. That is the difference between an OS that runs a proof and an OS you can run.
#
# The session has an exit-code contract because `halt` is a command: the guest halts via semihosting
# with 0, so a wedged console fails as a timeout rather than hanging a CI job forever.
#
# Two sessions against the SAME persistent disk:
#   1. write an object through the console, read it back, halt
#   2. boot again and `cat` it — what the operator typed survived a reboot
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# Honor the per-crate nightly via the rustup shim (a Homebrew cargo earlier in PATH ignores
# rust-toolchain.toml and cross-builds for the host — see scripts/vm-e2e.sh).
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi
KDIR="$ROOT/kernel"
TARGET="aarch64-unknown-none-softfloat"
ELF="$KDIR/target/$TARGET/debug/aletheia-kernel"

cd "$KDIR" || { echo "FAIL: no kernel dir"; exit 3; }

echo "==> building the kernel WITH the interactive console"
cargo build --features interactive || { echo "FAIL: build"; exit 3; }

IMG="$KDIR/target/console-scratch.img"
PIMG="$KDIR/target/console-persistent.img"
dd if=/dev/zero of="$IMG" bs=1048576 count=1 2>/dev/null || { echo "FAIL: scratch image"; exit 3; }
rm -f "$PIMG"
dd if=/dev/zero of="$PIMG" bs=1048576 count=1 2>/dev/null || { echo "FAIL: persistent image"; exit 3; }

# Type a line at the guest. The pause is not superstition: the console is POLLED, so the operator
# and the machine share a serial line with no flow control beyond the UART's own FIFO, and a human
# types slower than this.
type_lines() {
  sleep 4          # let the boot-time invariant suites finish before "typing"
  for line in "$@"; do
    printf '%s\r' "$line"
    sleep 1
  done
  sleep 2
}

boot_session() {
  type_lines "$@" | perl -e 'alarm 180; exec @ARGV or die' \
    qemu-system-aarch64 -machine virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 128M -nographic \
    -semihosting-config enable=on,target=native -kernel "$ELF" \
    -global virtio-mmio.force-legacy=false \
    -drive if=none,format=raw,file="$IMG",id=blk0 -device virtio-blk-device,drive=blk0 \
    -drive if=none,format=raw,file="$PIMG",id=blk1 -device virtio-blk-device,drive=blk1 \
    -netdev user,id=n0 -device virtio-net-device,netdev=n0
}

fail=0

echo "==> session 1: an operator writes an object through the console"
OUT1="$(boot_session "help" "arch" "mem" "write manifesto the OS you can sit in front of" "cat manifesto" "ls" "halt")"
CODE1=$?
echo "----------------------------------------"
echo "$OUT1"
echo "----------------------------------------"
echo "session 1 exit code: $CODE1 (expect 0)"

[ "$CODE1" -eq 0 ] || { echo "FAIL: the console session did not halt cleanly"; fail=1; }
echo "$OUT1" | grep -q "Aletheia interactive console" || { echo "FAIL: the console never started"; fail=1; }
echo "$OUT1" | grep -q "aletheia> "                   || { echo "FAIL: no prompt was printed"; fail=1; }
echo "$OUT1" | grep -q "commands:"                    || { echo "FAIL: help did not answer"; fail=1; }
echo "$OUT1" | grep -q "wrote 30 bytes to manifesto"  || { echo "FAIL: the write was not accepted"; fail=1; }
echo "$OUT1" | grep -q "the OS you can sit in front of" || { echo "FAIL: the object did not read back"; fail=1; }
echo "$OUT1" | grep -q "halting."                     || { echo "FAIL: halt did not run"; fail=1; }
echo "$OUT1" | grep -q "persistent virtio-blk device" || { echo "FAIL: the console did not choose the persistent disk"; fail=1; }

echo "==> session 2: a SECOND boot must still hold what the operator typed"
OUT2="$(boot_session "ls" "cat manifesto" "halt")"
CODE2=$?
echo "----------------------------------------"
echo "$OUT2"
echo "----------------------------------------"
echo "session 2 exit code: $CODE2 (expect 0)"

[ "$CODE2" -eq 0 ] || { echo "FAIL: the second console session did not halt cleanly"; fail=1; }
echo "$OUT2" | grep -q "the OS you can sit in front of" \
  || { echo "FAIL: what the operator wrote did NOT survive the reboot"; fail=1; }
if echo "$OUT2" | grep -q "no namespace on this device"; then
  echo "FAIL: the second boot reformatted the disk instead of mounting it"; fail=1
fi

if [ "$fail" -eq 0 ]; then
  echo "CONSOLE-E2E: PASS"
  exit 0
fi
echo "CONSOLE-E2E: FAIL"
exit 1
