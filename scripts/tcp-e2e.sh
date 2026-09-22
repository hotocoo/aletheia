#!/usr/bin/env bash
# The first LIVE TCP conversation, end to end (REQ-NET-006, ADR-140).
#
# Every other network gate proves this kernel can ARP, ping and exchange a datagram. This one
# proves the transport: a human types `tcp` at the console, the kernel opens a real connection
# through virtio-net to a server running on THIS host, sends what was typed, and prints what came
# back. Serial in, TCP out, TCP in, serial out.
#
# The peer is a real socket server on the host loopback. QEMU's user networking maps 10.0.2.2 to
# the host, so the guest dials a port this script opened moments earlier — nothing is emulated on
# the guest's behalf, and nothing about the answer is arranged by the gate except its text.
#
# A guest that cannot reach the peer must FAIL here rather than print a friendly refusal: a console
# that says "the peer did not answer" when a peer is answering is exactly the bug this gate exists
# to catch.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

command -v python3 >/dev/null 2>&1 || { echo "TCP-E2E: SKIP (python3 is needed for the peer)"; exit 0; }

# What the operator types, and what the peer answers with. The peer's answer is deliberately NOT
# an echo of the request: an echo cannot distinguish "the reply came back" from "the console
# printed what was typed".
REQUEST="hello-from-aletheia"
ANSWER_PREFIX="peer-saw:"

PEER_LOG="$(mktemp)"
PORT_FILE="$(mktemp)"
# The peer: one socket, bound to the loopback, answering every connection with what it was told,
# then closing so the guest's side walks its own shutdown path. Written to a file and run, rather
# than fed on stdin, because a here-document and a background job do not share stdin.
PEER_PY="$(mktemp)"
cat > "$PEER_PY" <<'PYSRC'
import socket, sys, threading

port_file = sys.argv[1]
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 0))
srv.listen(8)
with open(port_file, "w") as f:
    f.write(str(srv.getsockname()[1]))
srv.settimeout(600)

def serve(conn):
    conn.settimeout(30)
    try:
        data = b""
        while b"\n" not in data and len(data) < 4096:
            chunk = conn.recv(1024)
            if not chunk:
                break
            data += chunk
            if data:
                break
        print("peer received:", data, flush=True)
        conn.sendall(b"peer-saw:" + data + b"\n")
    except Exception as exc:                      # a guest may vanish mid-conversation; say so
        print("peer error:", exc, flush=True)
    finally:
        try:
            conn.shutdown(socket.SHUT_WR)
        except Exception:
            pass
        conn.close()

while True:
    try:
        conn, addr = srv.accept()
    except socket.timeout:
        break
    print("peer accepted:", addr, flush=True)
    threading.Thread(target=serve, args=(conn,), daemon=True).start()
PYSRC
python3 "$PEER_PY" "$PORT_FILE" >"$PEER_LOG" 2>&1 &
PEER_PID=$!

# Wait for the peer to publish its port.
PEER_PORT=""
for _ in $(seq 1 50); do
  PEER_PORT="$(cat "$PORT_FILE" 2>/dev/null)"
  [ -n "$PEER_PORT" ] && break
  sleep 0.2
done
if [ -z "$PEER_PORT" ]; then
  echo "TCP-E2E: FAIL (the peer never opened a port)"; cat "$PEER_LOG"; kill "$PEER_PID" 2>/dev/null; exit 1
fi
echo "==> the peer is listening on 127.0.0.1:$PEER_PORT (the guest dials 10.0.2.2:$PEER_PORT)"

cleanup() {
  kill "$PEER_PID" 2>/dev/null
  rm -f "$PORT_FILE" "$PEER_PY"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------------------------
# The scripted operator. Same discipline as scripts/console-e2e.sh: wait for the prompt, then type
# one line at a time, because a byte typed before the console exists is destroyed rather than
# queued.
# ---------------------------------------------------------------------------------------------
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
      [ "$spun" -ge 60 ] && break
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
  grep -q "byte(s) back"                 <<<"$log" || { echo "  FAIL [$label] the tcp command reported no answer"; bad=1; }
  grep -q "$ANSWER_PREFIX"               <<<"$log" || { echo "  FAIL [$label] the peer's answer never reached the console"; bad=1; }
  grep -q "$REQUEST"                     <<<"$log" || { echo "  FAIL [$label] the request text is missing from the answer"; bad=1; }
  # The refusal path must still be a refusal: a port nobody listens on is answered by name, not by
  # a hang and not by a pretend success.
  grep -q "tcp: the peer"                <<<"$log" || { echo "  FAIL [$label] dialing a dead port did not refuse by name"; bad=1; }
  # And the peer must have SEEN the request: without this, a console that printed its own input
  # would pass.
  grep -q "peer received: b'$REQUEST'" "$PEER_LOG" || { echo "  FAIL [$label] the peer never received the request"; bad=1; }
  [ "$bad" -eq 0 ] || fail=1
  return $bad
}

mmio_leg() {
  local label="$1" dir="$2" triple="$3" bin="$4"; shift 4
  local -a QEMU=("$@")
  local elf="$ROOT/$dir/target/$triple/debug/$bin"

  printf '========================================================================\n'
  echo "==> $label: building WITH the interactive console"
  printf '========================================================================\n'
  ( cd "$ROOT/$dir" && cargo build --features interactive ) || { echo "  FAIL [$label] build"; fail=1; return 1; }

  local img="$ROOT/$dir/target/tcp-scratch.img"
  dd if=/dev/zero of="$img" bs=1048576 count=1 2>/dev/null

  SESSION_ARGV=("${QEMU[@]}" -kernel "$elf"
    -global virtio-mmio.force-legacy=false
    -drive "if=none,format=raw,file=$img,id=blk0" -device virtio-blk-device,drive=blk0
    -netdev user,id=n0 -device virtio-net-device,netdev=n0
    -device virtio-rng-device)
  local log; log="$(mktemp)"

  # A dead port first (the refusal must be named), then the live peer.
  drive_session "$log" 240 "tcp 10.0.2.2 1 nobody-listens-here" \
    "tcp 10.0.2.2 $PEER_PORT $REQUEST" "halt"
  sed -n '/interactive console/,$p' "$log"
  check_transcript "$label" "$(cat "$log")"
  local rc=$?
  rm -f "$log"
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

printf '========================================================================\n'
echo "PEER TRANSCRIPT"
cat "$PEER_LOG"
printf '========================================================================\n'
for r in "${RESULTS[@]}"; do echo "  $r"; done
printf '========================================================================\n'
if [ "$fail" -eq 0 ]; then
  echo "TCP-E2E: PASS — a real connection, opened by a human, answered by a real peer"
  exit 0
fi
echo "TCP-E2E: FAIL"
exit 1
