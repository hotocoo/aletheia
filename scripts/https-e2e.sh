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
        elif self.path == "/index.html":
            body = (b"<!DOCTYPE html><html><head><title>Aletheia  Test</title>"
                    b"<script>alert('never shown')</script></head><body><h1>Welcome</h1>"
                    b"<p>See <a href=\"/plain.txt\">the plain page</a> for &amp; more.</p>"
                    b"<ul><li>one</li><li>two</li></ul></body></html>")
            self.send_response(200); self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body))); self.send_header("Connection", "close")
            self.end_headers(); self.wfile.write(body)
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

# A DNS stub for `trust NAME PIN` (ADR-178): it answers $SERVER_NAME with 10.0.2.2 (where the peer
# is reached), refuses every other name, and logs every question - so a blocked host that was asked
# about shows up here.
DNS_LOG="$(mktemp)"; DNS_PORT_FILE="$(mktemp)"
python3 - "$DNS_PORT_FILE" "$SERVER_NAME" > "$DNS_LOG" 2>&1 <<'PYDNS' &
import socket, struct, sys
port_file, served = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(("127.0.0.1", 0))
open(port_file, "w").write(str(s.getsockname()[1]))
s.settimeout(1800)
while True:
    try:
        msg, addr = s.recvfrom(512)
    except socket.timeout:
        break
    labels, at = [], 12
    while msg[at]:
        n = msg[at]; labels.append(msg[at + 1:at + 1 + n].decode()); at += 1 + n
    name = ".".join(labels).lower()
    print("dns query:", name, flush=True)
    question = msg[12:at + 5]
    if name == served:
        ans = b"\xc0\x0c" + struct.pack(">HHIH", 1, 1, 60, 4) + bytes([10, 0, 2, 2])
        reply = msg[:2] + struct.pack(">HHHHH", 0x8180, 1, 1, 0, 0) + question + ans
    else:
        reply = msg[:2] + struct.pack(">HHHHH", 0x8183, 1, 0, 0, 0) + question
    s.sendto(reply, addr)
PYDNS
DNS_PID=$!
DNS_PORT=""
for _ in $(seq 1 50); do DNS_PORT="$(cat "$DNS_PORT_FILE" 2>/dev/null)"; [ -n "$DNS_PORT" ] && break; sleep 0.2; done
[ -n "$DNS_PORT" ] || { echo "HTTPS-E2E: FAIL (the DNS stub never opened a port)"; exit 1; }
echo "==> the DNS stub answers on 127.0.0.1:$DNS_PORT (the guest asks 10.0.2.2:$DNS_PORT)"

cleanup() {
  kill "$PEER_PID" "$DNS_PID" 2>/dev/null
  rm -rf "$PORT_FILE" "$PEER_PY" "$WORK" "$DNS_PORT_FILE"
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
  # The browser's navigation (ADR-156): an unpinned host and plaintext refused before a dial, then a
  # trusted host's pages, then back to the first.
  grep -q "go: no root pinned" <<<"$log" || { echo "  FAIL [$label] an unpinned host was not refused before dialing"; bad=1; }
  grep -q "go: plaintext refused" <<<"$log" || { echo "  FAIL [$label] a plaintext URL was not refused"; bad=1; }
  grep -q "trust: $SERVER_NAME at 10.0.2.2" <<<"$log" || { echo "  FAIL [$label] trust did not pin the host"; bad=1; }
  # ADR-178: the address came from the nameserver; a blocked host was never asked about; a name the
  # server does not know pinned nothing.
  grep -q "trust: asked 10.0.2.2:$DNS_PORT for $SERVER_NAME" <<<"$log" || { echo "  FAIL [$label] trust NAME PIN did not ask the nameserver"; bad=1; }
  grep -q "trust: evil.test is blocked; its address was not asked for" <<<"$log" || { echo "  FAIL [$label] a blocked host was not refused before its lookup"; bad=1; }
  grep -q "dns query: evil.test" "$DNS_LOG" && { echo "  FAIL [$label] the nameserver was asked about a blocked host"; bad=1; }
  grep -q "trust: nowhere.test was not resolved: the server says that name does not exist" <<<"$log" || { echo "  FAIL [$label] an unknown name was not refused by name"; bad=1; }
  grep -q "trust: nowhere.test at" <<<"$log" && { echo "  FAIL [$label] a failed lookup pinned a host"; bad=1; }
  [ "$(grep -c "^HTTP 200 OK" <<<"$log")" -ge 3 ] || { echo "  FAIL [$label] the browser pages did not render their status lines (go, go, back)"; bad=1; }
  grep -q "^https://$SERVER_NAME:$PEER_PORT/chunked.txt" <<<"$log" || { echo "  FAIL [$label] the chunked page's URL line never rendered"; bad=1; }
  # The content renderer (ADR-158): the HTML page shows its heading, its link numbered and its
  # list, never its script; following the link fetches the plain page; a link that does not exist
  # is refused by name.
  grep -q "^Welcome" <<<"$log" || { echo "  FAIL [$label] the HTML page's heading never rendered"; bad=1; }
  grep -q "the plain page\[1\] for & mo" <<<"$log" || { echo "  FAIL [$label] the HTML page's link was not numbered and its entity not decoded"; bad=1; }
  grep -q "^\* one" <<<"$log" || { echo "  FAIL [$label] the HTML page's list did not render"; bad=1; }
  grep -q "alert(" <<<"$log" && { echo "  FAIL [$label] script content reached the page"; bad=1; }
  grep -q "peer request: /plain.txt" "$PEER_LOG" || { echo "  FAIL [$label] following the link never fetched the plain page"; bad=1; }
  grep -q "follow: the page offers no link \[7\]" <<<"$log" || { echo "  FAIL [$label] a link the page never offered was not refused"; bad=1; }
  # Lethe's policy contract (ADR-159), live: a host the person blocks is refused by name even though
  # it is pinned and was just fetched from, and nothing is dialed after the block; `forget` leaves
  # `back` nowhere to go.
  grep -q "^blocked $SERVER_NAME" <<<"$log" || { echo "  FAIL [$label] block did not take the host"; bad=1; }
  grep -q "go: that host is blocked" <<<"$log" || { echo "  FAIL [$label] a blocked pinned host was not refused by name"; bad=1; }
  local after_block; after_block="$(sed -n "/^aletheia> block $SERVER_NAME/,\$p" <<<"$log")"
  grep -q "^HTTP " <<<"$after_block" && { echo "  FAIL [$label] something was fetched after the block"; bad=1; }
  grep -q "forgotten: history, page and links" <<<"$log" || { echo "  FAIL [$label] forget did not answer"; bad=1; }
  grep -q "back: no previous page" <<<"$log" || { echo "  FAIL [$label] back after forget still had somewhere to go"; bad=1; }
  grep -q "peer request: /plain.txt host: $SERVER_NAME" "$PEER_LOG" || { echo "  FAIL [$label] the peer never saw the plain GET with its Host"; bad=1; }
  grep -q "peer request: /chunked.txt" "$PEER_LOG" || { echo "  FAIL [$label] the peer never saw the chunked GET"; bad=1; }
  grep -q "peer handshake: TLSv1.3" "$PEER_LOG" || { echo "  FAIL [$label] the peer never completed a TLS 1.3 handshake"; bad=1; }
  grep -q "peer handshake refused by client" "$PEER_LOG" || { echo "  FAIL [$label] the peer never saw the client refuse under the wrong pin"; bad=1; }
  [ "$bad" -eq 0 ] || fail=1
  return $bad
}

# The operator script every leg types: a dead port (refused by name), the live peer under a WRONG
# pin (refused by name), then the live peer under the right pin, then the browser.
SESSION_CMDS=(
  "https 10.0.2.2 1 $SERVER_NAME $PIN /plain.txt"
  "https 10.0.2.2 $PEER_PORT $SERVER_NAME $WRONG_PIN /plain.txt"
  "https 10.0.2.2 $PEER_PORT $SERVER_NAME $PIN /plain.txt"
  "https 10.0.2.2 $PEER_PORT $SERVER_NAME $PIN /chunked.txt"
  "https 10.0.2.2 $PEER_PORT $SERVER_NAME $PIN /big.txt"
  "https 10.0.2.2 $PEER_PORT $SERVER_NAME $PIN /missing.txt"
  "go https://$SERVER_NAME:$PEER_PORT/plain.txt"
  "nameserver 10.0.2.2 $DNS_PORT"
  "block evil.test"
  "trust evil.test $PIN"
  "trust nowhere.test $PIN"
  "trust $SERVER_NAME $PIN"
  "go http://$SERVER_NAME:$PEER_PORT/plain.txt"
  "go https://$SERVER_NAME:$PEER_PORT/plain.txt"
  "go https://$SERVER_NAME:$PEER_PORT/chunked.txt"
  "back"
  "go https://$SERVER_NAME:$PEER_PORT/index.html"
  "follow 1"
  "follow 7"
  "block $SERVER_NAME"
  "go https://$SERVER_NAME:$PEER_PORT/plain.txt"
  "forget"
  "back"
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
  drive_session "$log" 300 "${SESSION_CMDS[@]}"
  sed -n '/interactive console/,$p' "$log"
  check_transcript "$label" "$(cat "$log")"
  local rc=$?
  rm -f "$log"
  return $rc
}

# x86-64 (UEFI under OVMF, virtio-net-pci + virtio-rng-pci): the same session, the same checks.
# Until 2026-09-25 this target ran the identical TLS/HTTP code with no live peer at all.
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
  echo "HTTPS-E2E: PASS — a real HTTPS GET, typed by a human, verified under a pin they named, answered by a real HTTP server, both body framings read"
  exit 0
fi
echo "HTTPS-E2E: FAIL"
exit 1
