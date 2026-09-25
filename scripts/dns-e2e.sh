#!/usr/bin/env bash
# A name becomes an address, live (REQ-NET-007, ADR-176).
#
# The boot suite proves the resolver's reader on fixed messages. This gate proves the conversation:
# a human types `resolve` at the console, the kernel sends a real UDP query through virtio-net to a
# DNS server on THIS host (a Python stub, so every answer is known), and the answer - or its named
# refusal - comes back on the serial line. The stub answers each question differently, by name:
#
#   aletheia.test        two A records                    -> both printed, smaller TTL
#   www.aletheia.test    a CNAME to aletheia.test, then A -> the chain followed, 1 link
#   nx.aletheia.test     NXDOMAIN                          -> "does not exist", by name
#   spoof.aletheia.test  a reply with the WRONG query id   -> "someone else's question"
#   cut.aletheia.test    TC set (truncated)                -> "cut its answer short"
#   loop.aletheia.test   an answer whose name points at itself -> "names are malformed"
#
# Then one real question through QEMU's own resolver (10.0.2.3, forwarding to this host's), which
# SKIPs by name on a host that cannot resolve at all - never a silent pass.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi
command -v python3 >/dev/null 2>&1 || { echo "DNS-E2E: SKIP (python3 is needed for the stub server)"; exit 0; }

REAL_NAME="${DNS_REAL_NAME:-example.com}"
REAL_OK=0
python3 -c "import socket,sys; socket.gethostbyname(sys.argv[1])" "$REAL_NAME" 2>/dev/null && REAL_OK=1

PEER_LOG="$(mktemp)"
PORT_FILE="$(mktemp)"
PEER_PY="$(mktemp)"
cat > "$PEER_PY" <<'PYSRC'
import socket, struct, sys

port_file = sys.argv[1]
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(("127.0.0.1", 0))
open(port_file, "w").write(str(s.getsockname()[1]))
s.settimeout(900)

def qname(msg):
    labels, at = [], 12
    while msg[at]:
        n = msg[at]; labels.append(msg[at + 1:at + 1 + n].decode()); at += 1 + n
    return ".".join(labels).lower(), at + 5

def rr(owner, rtype, ttl, rdata):
    return owner + struct.pack(">HHIH", rtype, 1, ttl, len(rdata)) + rdata

def wire(name):
    return b"".join(bytes([len(l)]) + l.encode() for l in name.split(".")) + b"\0"

while True:
    try:
        msg, addr = s.recvfrom(512)
    except socket.timeout:
        break
    ident = struct.unpack(">H", msg[:2])[0]
    name, qend = qname(msg)
    question = msg[12:qend]
    print("stub query:", name, "id", ident, flush=True)
    flags, answers, rid = 0x8180, [], ident
    if name == "aletheia.test":
        answers = [rr(b"\xc0\x0c", 1, 60, bytes([10, 0, 2, 2])), rr(b"\xc0\x0c", 1, 30, bytes([10, 0, 2, 7]))]
    elif name == "www.aletheia.test":
        # CNAME www -> aletheia.test, then the A record for the target, owned by the target's name.
        target = wire("aletheia.test")
        answers = [rr(b"\xc0\x0c", 5, 60, target), rr(target, 1, 45, bytes([10, 0, 2, 2]))]
    elif name == "nx.aletheia.test":
        flags = 0x8183
    elif name == "spoof.aletheia.test":
        rid = (ident + 1) & 0xFFFF
        answers = [rr(b"\xc0\x0c", 1, 60, bytes([6, 6, 6, 6]))]
    elif name == "cut.aletheia.test":
        flags = 0x8380
    elif name == "loop.aletheia.test":
        # The answer's owner is a pointer to ITSELF: a reader without a bound would spin forever.
        owner_at = qend
        answers = [bytes([0xC0 | (owner_at >> 8), owner_at & 0xFF]) + struct.pack(">HHIH", 1, 1, 60, 4) + bytes([1, 2, 3, 4])]
    else:
        flags = 0x8183
    reply = struct.pack(">HHHHHH", rid, flags, 1, len(answers), 0, 0) + question + b"".join(answers)
    s.sendto(reply, addr)
PYSRC
python3 "$PEER_PY" "$PORT_FILE" > "$PEER_LOG" 2>&1 &
PEER_PID=$!
PEER_PORT=""
for _ in $(seq 1 50); do
  PEER_PORT="$(cat "$PORT_FILE" 2>/dev/null)"
  [ -n "$PEER_PORT" ] && break
  sleep 0.2
done
if [ -z "$PEER_PORT" ]; then
  echo "DNS-E2E: FAIL (the stub never opened a port)"; cat "$PEER_LOG"; kill "$PEER_PID" 2>/dev/null; exit 1
fi
echo "==> the DNS stub answers on 127.0.0.1:$PEER_PORT (the guest asks 10.0.2.2:$PEER_PORT)"
cleanup() {
  kill "$PEER_PID" 2>/dev/null
  rm -f "$PORT_FILE" "$PEER_PY"
}
trap cleanup EXIT

CONSOLE_RC=0
SESSION_ARGV=()
prompt_count() {
  local n
  n="$(grep -c "aletheia> " "$1" 2>/dev/null)"
  printf '%s' "${n:-0}"
}
drive_session() {
  local log="$1" boot_timeout="$2"; shift 2
  local fifo; fifo="$(mktemp -u)"
  mkfifo "$fifo"
  : > "$log"
  exec 3<>"$fifo"
  "${SESSION_ARGV[@]}" < "$fifo" > "$log" 2>&1 &
  local qpid=$!
  ( sleep "$boot_timeout"; kill -9 "$qpid" 2>/dev/null ) &
  local wpid=$!
  local waited=0
  while ! grep -q "aletheia> " "$log" 2>/dev/null; do
    kill -0 "$qpid" 2>/dev/null || break
    sleep 1
    waited=$((waited + 1))
    [ "$waited" -ge "$boot_timeout" ] && break
  done
  for line in "$@"; do
    local want cur spun=0
    want=$(( $(prompt_count "$log") + 1 ))
    printf '%s\r' "$line" >&3
    [ "$line" = "halt" ] && break
    while : ; do
      cur="$(prompt_count "$log")"
      [ "$cur" -ge "$want" ] && break
      kill -0 "$qpid" 2>/dev/null || break
      sleep 1
      spun=$((spun + 1))
      [ "$spun" -ge 120 ] && break
    done
  done
  wait "$qpid"; CONSOLE_RC=$?
  kill "$wpid" 2>/dev/null
  exec 3>&-
  rm -f "$fifo"
}

fail=0
declare -a RESULTS=()

check_transcript() {
  local label="$1" log="$2" bad=0
  grep -q "Aletheia interactive console" <<<"$log" || { echo "  FAIL [$label] the console never started"; bad=1; }
  grep -q "resolve aletheia.test: 10.0.2.2, 10.0.2.7 (ttl 30 s, 0 CNAME" <<<"$log" || { echo "  FAIL [$label] two A records were not both printed with the smaller TTL"; bad=1; }
  grep -q "resolve www.aletheia.test: 10.0.2.2 (ttl 45 s, 1 CNAME" <<<"$log" || { echo "  FAIL [$label] the CNAME chain was not followed"; bad=1; }
  grep -q "resolve: the server says that name does not exist" <<<"$log" || { echo "  FAIL [$label] NXDOMAIN was not refused by name"; bad=1; }
  grep -q "resolve: the answer is to someone else's question" <<<"$log" || { echo "  FAIL [$label] a spoofed id was not refused"; bad=1; }
  grep -q "6.6.6.6" <<<"$log" && { echo "  FAIL [$label] the spoofed address reached the console"; bad=1; }
  grep -q "resolve: the server cut its answer short" <<<"$log" || { echo "  FAIL [$label] a truncated answer was not refused by name"; bad=1; }
  grep -q "resolve: the answer's names are malformed" <<<"$log" || { echo "  FAIL [$label] a pointer loop was not refused"; bad=1; }
  grep -q "usage: resolve NAME" <<<"$log" || { echo "  FAIL [$label] a malformed name was not refused by usage"; bad=1; }
  if [ "$REAL_OK" = "1" ]; then
    grep -q "resolve $REAL_NAME: [0-9]" <<<"$log" || { echo "  FAIL [$label] the real resolver's answer for $REAL_NAME never reached the console"; bad=1; }
  else
    echo "  SKIP [$label] real-resolver leg: this host cannot resolve $REAL_NAME itself"
  fi
  grep -q "stub query: aletheia.test" "$PEER_LOG" || { echo "  FAIL [$label] the stub never saw the query"; bad=1; }
  [ "$bad" -eq 0 ] || fail=1
  return $bad
}

SESSION_CMDS=(
  "resolve aletheia.test 10.0.2.2 $PEER_PORT"
  "resolve www.aletheia.test 10.0.2.2 $PEER_PORT"
  "resolve nx.aletheia.test 10.0.2.2 $PEER_PORT"
  "resolve spoof.aletheia.test 10.0.2.2 $PEER_PORT"
  "resolve cut.aletheia.test 10.0.2.2 $PEER_PORT"
  "resolve loop.aletheia.test 10.0.2.2 $PEER_PORT"
  "resolve bad..name"
  "resolve $REAL_NAME"
  "halt"
)

mmio_leg() {
  local label="$1" dir="$2" triple="$3" bin="$4"; shift 4
  local -a QEMU=("$@")
  local elf="$ROOT/$dir/target/$triple/debug/$bin"

  printf '========================================================================\n'
  echo "==> $label: building WITH the interactive console"
  printf '========================================================================\n'
  ( cd "$ROOT/$dir" && cargo build --features interactive ) || { echo "  FAIL [$label] build"; fail=1; return 1; }

  local img="$ROOT/$dir/target/dns-scratch.img"
  dd if=/dev/zero of="$img" bs=1048576 count=1 2>/dev/null

  SESSION_ARGV=("${QEMU[@]}" -kernel "$elf"
    -global virtio-mmio.force-legacy=false
    -drive "if=none,format=raw,file=$img,id=blk0" -device virtio-blk-device,drive=blk0
    -netdev user,id=n0 -device virtio-net-device,netdev=n0
    -device virtio-rng-device)
  local log; log="$(mktemp)"

  # A dead port (refused by name), the live peer under a WRONG pin (refused by name), then the
  # live peer under the right pin.
  drive_session "$log" 300 "${SESSION_CMDS[@]}"
  sed -n '/interactive console/,$p' "$log"
  check_transcript "$label" "$(cat "$log")"
  local rc=$?
  rm -f "$log"
  return $rc
}

# x86-64 (UEFI under OVMF, virtio-net-pci + virtio-rng-pci): the same session, the same checks.
# Every CPU asks the same server the same questions.
x86_leg() {
  local label="x86-64" code="" vars=""
  for c in "${OVMF_CODE:-}" /opt/homebrew/share/qemu/edk2-x86_64-code.fd \
      /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd /usr/share/edk2/x64/OVMF_CODE.4m.fd; do
    [ -n "$c" ] && [ -f "$c" ] && { code="$c"; break; }
  done
  for v in "${OVMF_VARS:-}" /opt/homebrew/share/qemu/edk2-i386-vars.fd \
      /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd /usr/share/edk2/x64/OVMF_VARS.4m.fd; do
    [ -n "$v" ] && [ -f "$v" ] && { vars="$v"; break; }
  done
  if ! command -v qemu-system-x86_64 >/dev/null 2>&1 || ! command -v mformat >/dev/null 2>&1 \
      || [ -z "$code" ] || [ -z "$vars" ]; then
    echo "  x86-64 leg unavailable on this host (needs qemu-system-x86_64 + mtools + OVMF) — SKIPPED (never a silent pass)."
    return 2
  fi
  printf '========================================================================\n'
  echo "==> $label: building the UEFI image WITH the interactive console"
  printf '========================================================================\n'
  local img="$ROOT/kernel-x86_64/build/aletheia-x86_64-interactive.img"
  CARGO_FEATURES=interactive IMG="$img" "$ROOT/kernel-x86_64/scripts/build-image-linux.sh" \
    || { echo "  FAIL [$label] build"; fail=1; return 1; }
  local work; work="$(mktemp -d)"
  dd if=/dev/zero of="$work/scratch.img" bs=1048576 count=1 2>/dev/null
  dd if=/dev/zero of="$work/persist.img" bs=1048576 count=1 2>/dev/null
  cp "$vars" "$work/vars.fd"
  SESSION_ARGV=(qemu-system-x86_64 -machine q35 -m 256 -smp 4 -cpu qemu64,+smep -nographic
    -drive "if=pflash,format=raw,unit=0,file=$code,readonly=on"
    -drive "if=pflash,format=raw,unit=1,file=$work/vars.fd"
    -drive "format=raw,file=$img"
    -drive "if=none,format=raw,file=$work/scratch.img,id=blk0" -device virtio-blk-pci,drive=blk0
    -drive "if=none,format=raw,file=$work/persist.img,id=blk1" -device virtio-blk-pci,drive=blk1
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0
    -device virtio-rng-pci,disable-legacy=on
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 -no-reboot)
  local log="$work/serial.log"
  drive_session "$log" 400 "${SESSION_CMDS[@]}"
  sed -n '/interactive console/,$p' "$log"
  check_transcript "$label" "$(cat "$log")"
  local rc=$?
  rm -rf "$work"
  return $rc
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

x86_leg
x_rc=$?
case "$x_rc" in
  0) RESULTS+=("x86-64  : PASS") ;;
  2) RESULTS+=("x86-64  : SKIP") ;;
  *) RESULTS+=("x86-64  : FAIL") ;;
esac

printf '========================================================================\n'
echo "STUB TRANSCRIPT"
cat "$PEER_LOG"
printf '========================================================================\n'
for r in "${RESULTS[@]}"; do echo "  $r"; done
printf '========================================================================\n'
if [ "$fail" -eq 0 ]; then
  echo "DNS-E2E: PASS — names asked over real UDP, answers read by a bounded reader, every lie refused by name"
  exit 0
fi
echo "DNS-E2E: FAIL"
exit 1
