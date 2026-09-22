#!/usr/bin/env bash
# The first LIVE HTTPS request, end to end (REQ-WEB-001, ADR-155; Lethe stage N3).
#
# tls-e2e.sh proved the protected conversation. This gate proves the protocol on top of it: a human
# types `https` at the console with the name, the pin and a PATH; the kernel opens a TLS 1.3
# connection to a real HTTP server on THIS host (Python's `http.server` behind `ssl`, which is
# OpenSSL), sends a GET, and prints the status, the header count and the body the bounded reader
# produced. Two paths are fetched: one answered with Content-Length, one answered CHUNKED, so both
# body framings this client reads are proved against a server this tree did not write. A body
# larger than the console's buffer is fetched too, and must come back TRUNCATED and said so.
#
# The negatives stay: a wrong pin is refused by name before the request leaves the guest, and a
# port nobody listens on is refused by name.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

command -v python3 >/dev/null 2>&1 || { echo "HTTPS-E2E: SKIP (python3 is needed for the peer)"; exit 0; }
python3 -c "import cryptography, ssl" 2>/dev/null || {
  echo "HTTPS-E2E: SKIP (python3 needs the 'cryptography' package to issue the fixture certificate)"; exit 0; }

BODY_PLAIN="hello from a real http server over a real tls connection"
BODY_CHUNK="this body arrived in chunks and was reassembled by the client"
SERVER_NAME="aletheia.test"

WORK="$(mktemp -d)"
PIN="$(python3 "$ROOT/scripts/tls-fixtures.py" pem "$WORK")" || { echo "HTTPS-E2E: FAIL (fixture PEMs)"; exit 1; }
[ "${#PIN}" -eq 64 ] || { echo "HTTPS-E2E: FAIL (the pin is not 64 hex digits: '$PIN')"; exit 1; }
# One digit different: the same server, a root nobody trusts.
if [ "${PIN:0:1}" = "0" ]; then WRONG_PIN="1${PIN:1}"; else WRONG_PIN="0${PIN:1}"; fi

PEER_LOG="$(mktemp)"
PORT_FILE="$(mktemp)"
PEER_PY="$(mktemp)"
cat > "$PEER_PY" <<'PYSRC'
import ssl, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

port_file, cert, key = sys.argv[1], sys.argv[2], sys.argv[3]
PLAIN = b"hello from a real http server over a real tls connection"
CHUNK = b"this body arrived in chunks and was reassembled by the client"

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, fmt, *args):
        print("peer:", fmt % args, flush=True)
    def do_GET(self):
        print("peer request:", self.path, "host:", self.headers.get("Host"), flush=True)
        if self.path == "/plain.txt":
            self.send_response(200); self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(PLAIN))); self.send_header("Connection", "close")
            self.end_headers(); self.wfile.write(PLAIN)
        elif self.path == "/chunked.txt":
            self.send_response(200); self.send_header("Content-Type", "text/plain")
            self.send_header("Transfer-Encoding", "chunked"); self.send_header("Connection", "close")
            self.end_headers()
            for piece in (CHUNK[:10], CHUNK[10:31], CHUNK[31:]):
                self.wfile.write(b"%x\r\n" % len(piece) + piece + b"\r\n")
            self.wfile.write(b"0\r\n\r\n")
        elif self.path == "/big.txt":
            body = b"0123456789" * 400          # 4000 bytes: more than the console shows
            self.send_response(200); self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(body))); self.send_header("Connection", "close")
            self.end_headers(); self.wfile.write(body)
        else:
            self.send_response(404); self.send_header("Content-Length", "0"); self.send_header("Connection", "close")
            self.end_headers()

ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.minimum_version = ssl.TLSVersion.TLSv1_3
ctx.load_cert_chain(cert, key)

class TlsHandler(Handler):
    # The TLS handshake is done HERE, on the connection's own thread, so a client that refuses the
    # certificate and walks away cannot block the accept loop and starve every later request; the
    # outcome is printed, because the wrong-pin case is evidence only if the peer says it saw it.
    def setup(self):
        try:
            self.request = ctx.wrap_socket(self.request, server_side=True)
        except ssl.SSLError as exc:
            print("peer handshake refused by client:", exc, flush=True)
            raise
        except OSError as exc:
            print("peer handshake error:", exc, flush=True)
            raise
        print("peer handshake:", self.request.version(), self.request.cipher()[0], flush=True)
        super().setup()

class TlsHttpServer(ThreadingHTTPServer):
    daemon_threads = True
    def get_request(self):
        raw, addr = self.socket.accept()
        print("peer accepted:", addr, flush=True)
        return raw, addr
    def handle_error(self, request, client_address):
        pass                                  # the setup() above already said what happened

srv = TlsHttpServer(("127.0.0.1", 0), TlsHandler)
with open(port_file, "w") as f:
    f.write(str(srv.server_address[1]))
print("peer listening", flush=True)
srv.serve_forever()
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
  echo "HTTPS-E2E: FAIL (the peer never opened a port)"; cat "$PEER_LOG"; kill "$PEER_PID" 2>/dev/null; exit 1
fi
echo "==> the HTTPS peer is listening on 127.0.0.1:$PEER_PORT as $SERVER_NAME (the guest dials 10.0.2.2:$PEER_PORT)"
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
  grep -q "peer verified as $SERVER_NAME under the pin.*HTTP 200 OK; .* header(s); ${#BODY_PLAIN} byte(s) of body" <<<"$log" || { echo "  FAIL [$label] the plain GET did not report 200 with its body length"; bad=1; }
  grep -q "$BODY_PLAIN" <<<"$log" || { echo "  FAIL [$label] the plain body never reached the console"; bad=1; }
  grep -q "HTTP 200 OK; .* header(s); ${#BODY_CHUNK} byte(s) of body (chunked)" <<<"$log" || { echo "  FAIL [$label] the chunked GET did not report a reassembled chunked body"; bad=1; }
  grep -q "$BODY_CHUNK" <<<"$log" || { echo "  FAIL [$label] the chunked body never reached the console"; bad=1; }
  grep -q "HTTP 200 OK; .* header(s); 2048 byte(s) of body (truncated)" <<<"$log" || { echo "  FAIL [$label] the oversized body was not truncated and said so"; bad=1; }
  grep -q "HTTP 404" <<<"$log" || { echo "  FAIL [$label] a missing path did not report 404"; bad=1; }
  grep -q "https: the peer's certificate is not one the pinned root signed" <<<"$log" || { echo "  FAIL [$label] a wrong pin did not refuse by name"; bad=1; }
  grep -q "https: the peer refused or reset" <<<"$log" || { echo "  FAIL [$label] dialing a dead port did not refuse by name"; bad=1; }
  grep -q "peer request: /plain.txt host: $SERVER_NAME" "$PEER_LOG" || { echo "  FAIL [$label] the peer never saw the plain GET with its Host"; bad=1; }
  grep -q "peer request: /chunked.txt" "$PEER_LOG" || { echo "  FAIL [$label] the peer never saw the chunked GET"; bad=1; }
  grep -q "peer handshake: TLSv1.3" "$PEER_LOG" || { echo "  FAIL [$label] the peer never completed a TLS 1.3 handshake"; bad=1; }
  grep -q "peer handshake refused by client" "$PEER_LOG" || { echo "  FAIL [$label] the peer never saw the client refuse under the wrong pin"; bad=1; }
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

  local img="$ROOT/$dir/target/https-scratch.img"
  dd if=/dev/zero of="$img" bs=1048576 count=1 2>/dev/null

  SESSION_ARGV=("${QEMU[@]}" -kernel "$elf"
    -global virtio-mmio.force-legacy=false
    -drive "if=none,format=raw,file=$img,id=blk0" -device virtio-blk-device,drive=blk0
    -netdev user,id=n0 -device virtio-net-device,netdev=n0
    -device virtio-rng-device)
  local log; log="$(mktemp)"

  # A dead port (refused by name), the live peer under a WRONG pin (refused by name), then the
  # live peer under the right pin.
  drive_session "$log" 300 \
    "https 10.0.2.2 1 $SERVER_NAME $PIN /plain.txt" \
    "https 10.0.2.2 $PEER_PORT $SERVER_NAME $WRONG_PIN /plain.txt" \
    "https 10.0.2.2 $PEER_PORT $SERVER_NAME $PIN /plain.txt" \
    "https 10.0.2.2 $PEER_PORT $SERVER_NAME $PIN /chunked.txt" \
    "https 10.0.2.2 $PEER_PORT $SERVER_NAME $PIN /big.txt" \
    "https 10.0.2.2 $PEER_PORT $SERVER_NAME $PIN /missing.txt" \
    "halt"
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
  echo "HTTPS-E2E: PASS — a real HTTPS GET, typed by a human, verified under a pin they named, answered by a real HTTP server, both body framings read"
  exit 0
fi
echo "HTTPS-E2E: FAIL"
exit 1
