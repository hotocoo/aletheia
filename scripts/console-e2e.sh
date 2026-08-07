#!/usr/bin/env bash
# Interactive-console end-to-end gate, all three targets (REQ-CON-001, ADR-044).
#
# Every other gate proves the OS behaves while it BOOTS. This one proves it behaves while somebody
# is USING it: each kernel is built with `--features interactive`, so after the invariant suites it
# hands the machine to the serial line instead of exiting — and then a scripted operator types at
# it. That is the difference between an OS that runs a proof and an OS you can run.
#
# A session still has an exit-code contract, because `halt` is a command: the guest halts through the
# same path the gates use (semihosting / SiFive test / isa-debug-exit), so a wedged console fails as a
# timeout instead of hanging CI forever.
#
# Per target, TWO sessions against the SAME persistent disk:
#   1. an operator writes an object through the console, reads it back, halts
#   2. the machine boots again and `cat`s it — what was typed survived a reboot
#
# The x86-64 leg SKIPs (never silently passes) when the host lacks OVMF or mtools.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# Honor the per-crate nightly via the rustup shim (a Homebrew cargo earlier in PATH ignores
# rust-toolchain.toml and cross-builds for the host — see scripts/vm-e2e.sh).
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

# What the scripted operator types, and what must come back. The body is 30 bytes; the gate asserts
# the exact count so a silently truncated line cannot pass as a write.
BODY="the OS you can sit in front of"
WROTE="wrote 30 bytes to manifesto"

# Drive one session: boot the machine, WAIT FOR ITS PROMPT, then type.
#
# Waiting rather than sleeping is not politeness, it is correctness. A byte typed before the console
# exists is not merely early: on x86-64 the boot's `serial::init` CLEARS the receive FIFO, so those
# keystrokes are destroyed rather than queued, and a fixed sleep turns the whole gate into a race
# against however long the invariant suites take on that host. The operator here does what a human
# does — waits to see `aletheia> `, then types one line at a time.
#
# The caller puts the QEMU argv in SESSION_ARGV before calling. A nameref (`local -n`) would read
# better and does NOT work here: macOS ships bash 3.2, where namerefs do not exist and every leg
# fails with "local: -n: invalid option" — a global array is the portable spelling.
#
# $1 = log path, $2 = timeout seconds, rest = the lines to type.
# Sets CONSOLE_RC to the guest's exit code.
CONSOLE_RC=0
SESSION_ARGV=()
prompt_count() {
  # `grep -c` prints 0 and EXITS 1 when there is no match, so `|| echo 0` would print a SECOND zero
  # and every later `[ "$n" -ge ... ]` would die with "integer expression expected".
  local n
  n="$(grep -c "aletheia> " "$1" 2>/dev/null)"
  printf '%s' "${n:-0}"
}
drive_session() {
  local log="$1" boot_timeout="$2"; shift 2
  local fifo; fifo="$(mktemp -u)"
  mkfifo "$fifo"
  : > "$log"

  # Hold the FIFO open from this shell, so QEMU never sees EOF between lines.
  exec 3<>"$fifo"
  "${SESSION_ARGV[@]}" < "$fifo" > "$log" 2>&1 &
  local qpid=$!
  ( sleep "$boot_timeout"; kill -9 "$qpid" 2>/dev/null ) &
  local wpid=$!

  # The machine is ready when it has printed a prompt.
  local waited=0
  while ! grep -q "aletheia> " "$log" 2>/dev/null; do
    kill -0 "$qpid" 2>/dev/null || break        # it died; the assertions will say why
    sleep 1
    waited=$((waited + 1))
    [ "$waited" -ge "$boot_timeout" ] && break
  done

  for line in "$@"; do
    # Sample the prompt count BEFORE typing, not after. Sampling after is a race: if the guest has
    # already answered and printed its next prompt by the time the sample is taken, `want` is one too
    # high, that command spins its full timeout, and every later command inherits the skew — six
    # commands x 30s is exactly the watchdog, which is how this presented (a session that answered
    # every command and then "never received" the last one).
    local want cur spun=0
    want=$(( $(prompt_count "$log") + 1 ))
    printf '%s\r' "$line" >&3
    # `halt` is the one command that does NOT print another prompt — waiting for one would just
    # burn the timeout on every session.
    [ "$line" = "halt" ] && break
    while : ; do
      cur="$(prompt_count "$log")"
      [ "$cur" -ge "$want" ] && break
      kill -0 "$qpid" 2>/dev/null || break
      sleep 1
      spun=$((spun + 1))
      [ "$spun" -ge 30 ] && break
    done
  done

  wait "$qpid"; CONSOLE_RC=$?
  kill "$wpid" 2>/dev/null
  exec 3>&-
  rm -f "$fifo"
}

fail=0
declare -a RESULTS=()

# Assert one session's transcript. $1 = label, $2 = exit code, $3 = expected code, $4 = log,
# $5 = "first" | "second".
check_session() {
  local label="$1" code="$2" want="$3" log="$4" phase="$5" bad=0
  [ "$code" -eq "$want" ] || { echo "  FAIL [$label/$phase] exit $code, expected $want"; bad=1; }
  grep -q "Aletheia interactive console" <<<"$log" || { echo "  FAIL [$label/$phase] the console never started"; bad=1; }
  grep -q "aletheia> "                   <<<"$log" || { echo "  FAIL [$label/$phase] no prompt was printed"; bad=1; }
  grep -q "halting."                     <<<"$log" || { echo "  FAIL [$label/$phase] halt did not run"; bad=1; }
  grep -q "persistent virtio-blk device" <<<"$log" || { echo "  FAIL [$label/$phase] the console did not choose the persistent disk"; bad=1; }
  if [ "$phase" = "first" ]; then
    grep -q "commands:" <<<"$log" || { echo "  FAIL [$label/first] help did not answer"; bad=1; }
    grep -q "$WROTE"    <<<"$log" || { echo "  FAIL [$label/first] the write was not accepted"; bad=1; }
    grep -q "$BODY"     <<<"$log" || { echo "  FAIL [$label/first] the object did not read back"; bad=1; }
  else
    grep -q "$BODY" <<<"$log" \
      || { echo "  FAIL [$label/second] what the operator wrote did NOT survive the reboot"; bad=1; }
    if grep -q "no namespace on this device" <<<"$log"; then
      echo "  FAIL [$label/second] the second boot reformatted the disk instead of mounting it"; bad=1
    fi
  fi
  [ "$bad" -eq 0 ] || fail=1
  return $bad
}

hr() { printf '========================================================================\n'; }

# ---------------------------------------------------------------------------------------------
# aarch64 and RISC-V: direct `-kernel` boots, serial on stdio, exit code straight from the guest
# ---------------------------------------------------------------------------------------------
mmio_leg() {
  # $1 = label, $2 = crate dir, $3 = target triple, $4 = binary name, then the qemu argv prefix.
  local label="$1" dir="$2" triple="$3" bin="$4"; shift 4
  local -a QEMU=("$@")
  local elf="$ROOT/$dir/target/$triple/debug/$bin"

  hr; echo "==> $label: building WITH the interactive console"; hr
  ( cd "$ROOT/$dir" && cargo build --features interactive ) || { echo "  FAIL [$label] build"; fail=1; return 1; }

  local img="$ROOT/$dir/target/console-scratch.img"
  local pimg="$ROOT/$dir/target/console-persistent.img"
  dd if=/dev/zero of="$img" bs=1048576 count=1 2>/dev/null
  rm -f "$pimg"
  dd if=/dev/zero of="$pimg" bs=1048576 count=1 2>/dev/null

  SESSION_ARGV=("${QEMU[@]}" -kernel "$elf"
    -global virtio-mmio.force-legacy=false
    -drive "if=none,format=raw,file=$img,id=blk0" -device virtio-blk-device,drive=blk0
    -drive "if=none,format=raw,file=$pimg,id=blk1" -device virtio-blk-device,drive=blk1
    -netdev user,id=n0 -device virtio-net-device,netdev=n0)
  local log; log="$(mktemp)"

  echo "--> session 1: an operator writes an object through the console"
  drive_session "$log" 180 "help" "arch" "mem" "write manifesto $BODY" "cat manifesto" "ls" "halt"
  sed -n '/interactive console/,$p' "$log"
  check_session "$label" "$CONSOLE_RC" 0 "$(cat "$log")" first
  local s1=$?

  echo "--> session 2: a SECOND boot must still hold what the operator typed"
  drive_session "$log" 180 "ls" "cat manifesto" "halt"
  sed -n '/interactive console/,$p' "$log"
  check_session "$label" "$CONSOLE_RC" 0 "$(cat "$log")" second
  local s2=$?

  rm -f "$log"
  return $((s1 + s2))
}

mmio_leg "aarch64" kernel aarch64-unknown-none-softfloat aletheia-kernel \
  qemu-system-aarch64 -machine virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 128M -nographic \
  -semihosting-config enable=on,target=native
a_rc=$?
RESULTS+=("aarch64 : $([ "$a_rc" -eq 0 ] && echo PASS || echo FAIL)")

mmio_leg "riscv64" kernel-riscv64 riscv64gc-unknown-none-elf aletheia-kernel-riscv64 \
  qemu-system-riscv64 -machine virt -cpu rv64 -smp 4 -m 128M -nographic -bios default
r_rc=$?
RESULTS+=("riscv64 : $([ "$r_rc" -eq 0 ] && echo PASS || echo FAIL)")

# ---------------------------------------------------------------------------------------------
# x86-64: a UEFI disk image under OVMF. Serial is stdio here too, so the same operator script works;
# the exit code is isa-debug-exit's, which cannot emit 0 — success is 33.
# ---------------------------------------------------------------------------------------------
x86_leg() {
  local label="x86-64"
  local have_ovmf=0 code=""
  for c in "${OVMF_CODE:-}" /opt/homebrew/share/qemu/edk2-x86_64-code.fd \
           /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd \
           /usr/share/edk2/x64/OVMF_CODE.4m.fd; do
    [ -n "$c" ] && [ -f "$c" ] && { code="$c"; have_ovmf=1; break; }
  done
  local vars=""
  for v in "${OVMF_VARS:-}" /opt/homebrew/share/qemu/edk2-i386-vars.fd \
           /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd \
           /usr/share/edk2/x64/OVMF_VARS.4m.fd; do
    [ -n "$v" ] && [ -f "$v" ] && { vars="$v"; break; }
  done
  if ! command -v qemu-system-x86_64 >/dev/null 2>&1 || ! command -v mformat >/dev/null 2>&1 \
     || [ "$have_ovmf" = "0" ] || [ -z "$vars" ]; then
    echo "  x86-64 console leg unavailable on this host (needs qemu-system-x86_64 + mtools + OVMF) — SKIPPED (never a silent pass)."
    RESULTS+=("x86-64  : SKIP")
    return 0
  fi

  hr; echo "==> $label: building the UEFI image WITH the interactive console"; hr
  local img="$ROOT/kernel-x86_64/build/aletheia-x86_64-interactive.img"
  CARGO_FEATURES=interactive IMG="$img" bash "$ROOT/kernel-x86_64/scripts/build-image-linux.sh" \
    || { echo "  FAIL [$label] image build"; fail=1; RESULTS+=("x86-64  : FAIL"); return 1; }

  local work; work="$(mktemp -d)"
  local scratch="$work/scratch.img" persist="$work/persist.img"
  dd if=/dev/zero of="$scratch" bs=1048576 count=1 2>/dev/null
  dd if=/dev/zero of="$persist" bs=1048576 count=1 2>/dev/null

  # OVMF rewrites its NVRAM, so each boot gets a fresh copy of the template; the DISKS are kept,
  # which is what makes the second boot a real reboot of the same machine.
  cp "$vars" "$work/vars.fd"
  SESSION_ARGV=(qemu-system-x86_64 -machine q35 -m 256 -smp 4 -cpu qemu64,+smep -nographic
    -drive "if=pflash,format=raw,unit=0,file=$code,readonly=on"
    -drive "if=pflash,format=raw,unit=1,file=$work/vars.fd"
    -drive "format=raw,file=$img"
    -drive "if=none,format=raw,file=$scratch,id=blk0" -device virtio-blk-pci,drive=blk0
    -drive "if=none,format=raw,file=$persist,id=blk1" -device virtio-blk-pci,drive=blk1
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 -no-reboot)
  local log; log="$work/serial.log"

  echo "--> session 1: an operator writes an object through the console"
  drive_session "$log" 180 "help" "arch" "mem" "write manifesto $BODY" "cat manifesto" "ls" "halt"
  sed -n '/interactive console/,$p' "$log"
  check_session "$label" "$CONSOLE_RC" 33 "$(cat "$log")" first
  local s1=$?

  echo "--> session 2: a SECOND boot must still hold what the operator typed"
  cp "$vars" "$work/vars.fd"
  drive_session "$log" 180 "ls" "cat manifesto" "halt"
  sed -n '/interactive console/,$p' "$log"
  check_session "$label" "$CONSOLE_RC" 33 "$(cat "$log")" second
  local s2=$?

  rm -rf "$work"
  RESULTS+=("x86-64  : $([ $((s1 + s2)) -eq 0 ] && echo PASS || echo FAIL)")
}

x86_leg

hr
echo "CONSOLE-E2E SUMMARY"
hr
for r in "${RESULTS[@]}"; do echo "  $r"; done
hr
if [ "$fail" -eq 0 ]; then
  echo "CONSOLE-E2E: PASS"
  exit 0
fi
echo "CONSOLE-E2E: FAIL"
exit 1
