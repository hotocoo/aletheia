#!/usr/bin/env bash
# The first LIVE TLS 1.3 conversation, end to end (REQ-SEC-TLS-010, ADR-151).
#
# tcp-e2e.sh proved the transport. This gate proves the thing on top of it: a human types `tls` at
# the console with the DNS name the peer must speak for and the Ed25519 root they trust, the kernel
# opens a real connection through virtio-net to a real TLS 1.3 server on THIS host (Python's `ssl`,
# which is OpenSSL), verifies the server's certificate under that pin at the time its own clock
# reads, checks the server's CertificateVerify and Finished, sends what was typed PROTECTED, and
# prints the protected answer. Serial in, TLS out, TLS in, serial out.
#
# The server's certificate is the tree's fixture leaf, issued by the fixture root
# (scripts/tls-fixtures.py); the pin the operator types is that root's public key. The negative is
# proved too: the same server dialed under a pin one digit different is refused BY NAME before a
# byte of the request leaves this machine, and a port nobody listens on is refused by name as well.
#
# A guest that cannot reach the peer must FAIL here rather than print a friendly refusal.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

command -v python3 >/dev/null 2>&1 || { echo "TLS-E2E: SKIP (python3 is needed for the peer)"; exit 0; }
python3 -c "import cryptography, ssl" 2>/dev/null || {
  echo "TLS-E2E: SKIP (python3 needs the 'cryptography' package to issue the fixture certificate)"; exit 0; }

REQUEST="hello-over-tls-from-aletheia"
ANSWER_PREFIX="peer-saw:"
SERVER_NAME="aletheia.test"

WORK="$(mktemp -d)"
PIN="$(python3 "$ROOT/scripts/tls-fixtures.py" pem "$WORK")" || { echo "TLS-E2E: FAIL (fixture PEMs)"; exit 1; }
[ "${#PIN}" -eq 64 ] || { echo "TLS-E2E: FAIL (the pin is not 64 hex digits: '$PIN')"; exit 1; }
# One digit different: the same server, a root nobody trusts.
if [ "${PIN:0:1}" = "0" ]; then WRONG_PIN="1${PIN:1}"; else WRONG_PIN="0${PIN:1}"; fi

PEER_LOG="$(mktemp)"
PORT_FILE="$(mktemp)"
PEER_PY="$(mktemp)"
cat > "$PEER_PY" <<'PYSRC'
import socket, ssl, sys, threading

port_file, cert, key = sys.argv[1], sys.argv[2], sys.argv[3]
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.minimum_version = ssl.TLSVersion.TLSv1_3
ctx.load_cert_chain(cert, key)
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 0))
srv.listen(8)
with open(port_file, "w") as f:
    f.write(str(srv.getsockname()[1]))
srv.settimeout(600)

def serve(raw):
    raw.settimeout(30)
    try:
        conn = ctx.wrap_socket(raw, server_side=True)
    except ssl.SSLError as exc:
        # A client that refuses the handshake shows up HERE, as the alert or the closed socket
        # it left behind: the negative case's evidence.
        print("peer handshake refused by client:", exc, flush=True)
        raw.close()
        return
    except Exception as exc:
        print("peer handshake error:", exc, flush=True)
        raw.close()
        return
    try:
        print("peer handshake:", conn.version(), conn.cipher()[0], flush=True)
        data = conn.recv(1024)
        print("peer received:", data, flush=True)
        conn.sendall(b"peer-saw:" + data + b"\n")
    except Exception as exc:
        print("peer error:", exc, flush=True)
    finally:
        try:
            conn.unwrap()          # close_notify, so the client sees an orderly end
        except Exception:
            pass
        try:
            conn.close()
        except Exception:
            pass

while True:
    try:
        raw, addr = srv.accept()
    except socket.timeout:
        break
    print("peer accepted:", addr, flush=True)
    threading.Thread(target=serve, args=(raw,), daemon=True).start()
PYSRC
python3 "$PEER_PY" "$PORT_FILE" "$WORK/leaf.pem" "$WORK/leaf-key.pem" >"$PEER_LOG" 2>&1 &
PEER_PID=$!

PEER_PORT=""
for _ in $(seq 1 50); do
  PEER_PORT="$(cat "$PORT_FILE" 2>/dev/null)"
  [ -n "$PEER_PORT" ] && break
  sleep 0.2
done
if [ -z "$PEER_PORT" ]; then
  echo "TLS-E2E: FAIL (the peer never opened a port)"; cat "$PEER_LOG"; kill "$PEER_PID" 2>/dev/null; exit 1
fi
echo "==> the TLS peer is listening on 127.0.0.1:$PEER_PORT as $SERVER_NAME (the guest dials 10.0.2.2:$PEER_PORT)"
echo "==> the pin the operator types: $PIN"

cleanup() {
  kill "$PEER_PID" 2>/dev/null
  rm -rf "$PORT_FILE" "$PEER_PY" "$WORK"
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
  grep -q "peer verified as $SERVER_NAME under the pin" <<<"$log" || { echo "  FAIL [$label] the tls command never reported a verified peer"; bad=1; }
  grep -q "byte(s) back"                 <<<"$log" || { echo "  FAIL [$label] the tls command reported no answer"; bad=1; }
  grep -q "$ANSWER_PREFIX$REQUEST"       <<<"$log" || { echo "  FAIL [$label] the peer's protected answer never reached the console"; bad=1; }
  # The negative: the same server under a pin one digit off is refused by name, and nothing
  # protected was sent to it (the peer saw a handshake the client refused, not a request).
  grep -q "tls: the peer's certificate is not one the pinned root signed" <<<"$log" || { echo "  FAIL [$label] a wrong pin did not refuse by name"; bad=1; }
  grep -q "tls: the peer refused or reset" <<<"$log" || { echo "  FAIL [$label] dialing a dead port did not refuse by name"; bad=1; }
  # The peer's own record: a TLS 1.3 handshake, the request received in the clear only on ITS side.
  grep -q "peer handshake: TLSv1.3" "$PEER_LOG" || { echo "  FAIL [$label] the peer never completed a TLS 1.3 handshake"; bad=1; }
  grep -q "peer received: b'$REQUEST'" "$PEER_LOG" || { echo "  FAIL [$label] the peer never received the request"; bad=1; }
  grep -q "peer handshake refused by client" "$PEER_LOG" || { echo "  FAIL [$label] the peer never saw the client refuse under the wrong pin"; bad=1; }
  [ "$bad" -eq 0 ] || fail=1
  return $bad
}

# The operator script every leg types: a dead port (refused by name), the live peer under a WRONG
# pin (refused by name), then the live peer under the right pin.
SESSION_CMDS=(
  "tls 10.0.2.2 1 $SERVER_NAME $PIN nobody-listens-here"
  "tls 10.0.2.2 $PEER_PORT $SERVER_NAME $WRONG_PIN not-for-you"
  "tls 10.0.2.2 $PEER_PORT $SERVER_NAME $PIN $REQUEST"
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

  local img="$ROOT/$dir/target/tls-scratch.img"
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
# Until 2026-09-25 this target ran the identical TLS code with no live peer at all.
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
echo "PEER TRANSCRIPT"
cat "$PEER_LOG"
printf '========================================================================\n'
for r in "${RESULTS[@]}"; do echo "  $r"; done
printf '========================================================================\n'
if [ "$fail" -eq 0 ]; then
  echo "TLS-E2E: PASS — a real TLS 1.3 conversation, opened by a human, verified under a pin they named, answered by OpenSSL"
  exit 0
fi
echo "TLS-E2E: FAIL"
exit 1
