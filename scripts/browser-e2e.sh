#!/usr/bin/env bash
# The browser window, LIVE (ADR-160): a person types a URL into the desktop's browser window with a
# real virtio keyboard, and the page a real HTTPS server answered appears in that window.
#
# This is the path ADR-157 built and ADR-156/158 proved only from the console: Alt+5 focuses the
# window, keystrokes reach its URL line through the compositor's per-window queue, Enter latches
# the URL, the console session collects it on its idle turn, navigates through its own trust
# table (filled here by `trust` on the serial line), dials TLS 1.3 to the peer on THIS host,
# renders the answer and pushes it back into the window. Every step is asked of the machine through
# the console's `input` readout, which reads the window's own state (ADR-160), and of the peer,
# which logs what it was asked. Both device-tree targets; x86-64 runs the identical code path
# behind its own desktop gate.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi
command -v python3 >/dev/null 2>&1 || { echo "BROWSER-E2E: SKIP (python3 is needed for the peer and the driver)"; exit 0; }
python3 -c "import cryptography, ssl" 2>/dev/null || {
  echo "BROWSER-E2E: SKIP (python3 needs the 'cryptography' package to issue the fixture certificate)"; exit 0; }

SERVER_NAME="aletheia.test"
WORK="$(mktemp -d)"
PIN="$(python3 "$ROOT/scripts/tls-fixtures.py" pem "$WORK")" || { echo "BROWSER-E2E: FAIL (fixture PEMs)"; exit 1; }
[ "${#PIN}" -eq 64 ] || { echo "BROWSER-E2E: FAIL (the pin is not 64 hex digits: '$PIN')"; exit 1; }

PEER_LOG="$(mktemp)"
PORT_FILE="$(mktemp)"
PEER_PY="$(mktemp)"
cat > "$PEER_PY" <<'PYSRC'
import ssl, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
port_file, cert, key = sys.argv[1], sys.argv[2], sys.argv[3]
PLAIN = b"a page that reached the window"
class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, fmt, *args):
        print("peer:", fmt % args, flush=True)
    def do_GET(self):
        print("peer request:", self.path, "host:", self.headers.get("Host"), "agent:", self.headers.get("User-Agent"), flush=True)
        if self.path == "/index.html":
            body = (b"<html><head><title>Index</title></head><body><h1>Index</h1>"
                    b"<p><a href=\"/plain.txt\">the plain page</a></p></body></html>")
            self.send_response(200); self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body))); self.send_header("Connection", "close")
            self.end_headers(); self.wfile.write(body); return
        body = PLAIN if self.path == "/plain.txt" else b""
        self.send_response(200 if body else 404); self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body))); self.send_header("Connection", "close")
        self.end_headers(); self.wfile.write(body)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.minimum_version = ssl.TLSVersion.TLSv1_3
ctx.load_cert_chain(cert, key)
class TlsHandler(Handler):
    def setup(self):
        try:
            self.request = ctx.wrap_socket(self.request, server_side=True)
        except (ssl.SSLError, OSError) as exc:
            print("peer handshake failed:", exc, flush=True)
            raise
        print("peer handshake:", self.request.version(), flush=True)
        super().setup()
class TlsHttpServer(ThreadingHTTPServer):
    daemon_threads = True
    def handle_error(self, request, client_address):
        pass
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
  echo "BROWSER-E2E: FAIL (the peer never opened a port)"; cat "$PEER_LOG"; kill "$PEER_PID" 2>/dev/null; exit 1
fi
echo "==> the HTTPS peer is listening on 127.0.0.1:$PEER_PORT as $SERVER_NAME (the guest dials 10.0.2.2:$PEER_PORT)"

cleanup() {
  kill "$PEER_PID" 2>/dev/null
  rm -rf "$PORT_FILE" "$PEER_PY" "$WORK" "$PEER_LOG"
}
trap cleanup EXIT

fail=0
run_target() {
  local arch="$1" dir qemu machine cpu target elf img qmp ser log rootbin dtbraw dtb
  if [ "$arch" = aarch64 ]; then
    dir="$ROOT/kernel"; qemu=qemu-system-aarch64; target=aarch64-unknown-none-softfloat
    machine='virt,iommu=smmuv3,highmem-ecam=off,gic-version=2'; cpu=cortex-a72
    elf="$dir/target/$target/debug/aletheia-kernel"
  else
    dir="$ROOT/kernel-riscv64"; qemu=qemu-system-riscv64; target=riscv64gc-unknown-none-elf
    machine=virt; cpu=rv64; elf="$dir/target/$target/debug/aletheia-kernel-riscv64"
  fi
  command -v "$qemu" >/dev/null 2>&1 || { echo "BROWSER-E2E-$arch: SKIP (missing $qemu)"; return 0; }
  echo "==> [$arch] build interactive kernel"
  (cd "$dir" && cargo build --features interactive) || { echo "BROWSER-E2E-$arch: FAIL (build)"; fail=1; return 1; }
  img="$dir/target/browser-e2e.img"
  dd if=/dev/zero of="$img" bs=1048576 count=1 2>/dev/null
  # The aarch64 boot suite proves the SMMUv3 rung against a virtio-blk-pci function behind the
  # unit (ADR-074); the machine must carry one or that suite fails before the console starts.
  local pciimg="$dir/target/browser-e2e-pci.img"
  dd if=/dev/zero of="$pciimg" bs=1048576 count=1 2>/dev/null
  qmp="${TMPDIR:-/tmp}/aletheia-browser-$arch-$$.qmp"; ser="${TMPDIR:-/tmp}/aletheia-browser-$arch-$$.ser"
  log="$dir/target/browser-e2e-$arch.log"; rm -f "$qmp" "$ser" "$log"
  rootbin="$dir/target/capvault-root.bin"
  printf 'aletheia-capvault-root-0123456789abcdef' | head -c 32 > "$rootbin"
  # The desktop's devices AND the network's, on one machine: the window path needs both.
  local -a devices=(-device virtio-gpu-device -device virtio-keyboard-device -device virtio-tablet-device
    -drive "if=none,format=raw,file=$pciimg,id=pciblk0" -device virtio-blk-pci,disable-legacy=on,drive=pciblk0
    -netdev user,id=n0 -device virtio-net-device,netdev=n0 -device virtio-rng-device)
  local -a args=("$qemu" -machine "$machine" -cpu "$cpu" -smp 4 -m 128M -display none -S
    -global virtio-mmio.force-legacy=false
    -drive "if=none,format=raw,file=$img,id=blk0" -device virtio-blk-device,drive=blk0
    "${devices[@]}"
    -kernel "$elf" -chardev "socket,id=ser0,path=$ser,server=on,wait=off" -serial chardev:ser0
    -qmp "unix:$qmp,server,nowait")
  if [ "$arch" = aarch64 ]; then
    dtbraw="$dir/target/browser-e2e-dtb-raw.bin"; dtb="$dir/target/browser-e2e-dtb.bin"
    "$qemu" -machine "$machine" -global arm-smmuv3.stage=2 -cpu "$cpu" -smp 4 -m 128M \
      -global virtio-mmio.force-legacy=false \
      -drive "if=none,format=raw,file=$img,id=blk0" -device virtio-blk-device,drive=blk0 \
      "${devices[@]}" \
      -machine dumpdtb="$dtbraw" >/dev/null 2>&1
    set -- $(od -An -tu1 -j4 -N4 "$dtbraw")
    local tsz=$(( $1 << 24 | $2 << 16 | $3 << 8 | $4 )); head -c "$tsz" "$dtbraw" > "$dtb"
    args+=( -global arm-smmuv3.stage=2 -fw_cfg "name=opt/org.aletheia/capvault-root,file=$rootbin" -fw_cfg "name=opt/org.aletheia/dtb,file=$dtb" )
  else
    args+=( -bios default )
  fi

  "${args[@]}" & local pid=$!
  trap 'kill -9 "$pid" 2>/dev/null || true; rm -f "$qmp" "$ser"' RETURN

  python3 - "$qmp" "$ser" "$log" "$SERVER_NAME" "$PEER_PORT" "$PIN" "$PEER_LOG" <<'PY'
import json, re, socket, sys, threading, time
qmp, serial, log, name, port, pin, peer_log = sys.argv[1:]
def conn(path):
    end=time.time()+60
    while time.time()<end:
        s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
        try: s.connect(path); return s
        except OSError: s.close(); time.sleep(.1)
    raise RuntimeError('socket timeout: '+path)
q=conn(qmp); qf=q.makefile('rwb'); qf.readline()
def qcmd(x):
    qf.write((json.dumps(x)+'\n').encode()); qf.flush()
    while True:
        m=json.loads(qf.readline())
        if 'return' in m or 'error' in m: return m
s=conn(serial); lf=open(log,'ab')
def rd():
    while True:
        try: b=s.recv(4096)
        except OSError: return
        if not b: return
        lf.write(b); lf.flush()
threading.Thread(target=rd,daemon=True).start()
def txt():
    try: return open(log,'rb').read().decode('utf-8','replace')
    except FileNotFoundError: return ''
def wait(needle, sec=180):
    end=time.time()+sec
    while time.time()<end:
        if needle in txt(): return True
        time.sleep(.2)
    return False
qcmd({'execute':'qmp_capabilities'}); qcmd({'execute':'cont'})
if not wait('Aletheia interactive console',240) or not wait('aletheia>',60):
    raise RuntimeError('interactive console/prompt did not start')
def send(events):
    r=qcmd({'execute':'input-send-event','arguments':{'events':events}})
    if 'error' in r: raise RuntimeError(str(r['error']))
    time.sleep(.15)
def down(k): return {'type':'key','data':{'down':True,'key':{'type':'qcode','data':k}}}
def up(k):   return {'type':'key','data':{'down':False,'key':{'type':'qcode','data':k}}}
def key(k, shift=False, ctrl=False):
    ev=[down(k),up(k)]
    if shift: ev=[down('shift')]+ev+[up('shift')]
    if ctrl: ev=[down('ctrl')]+ev+[up('ctrl')]
    send(ev)
QCODE={'/':'slash','.':'dot','-':'minus',':':('semicolon',True),' ':'spc','\n':'ret'}
def type_text(t):
    for c in t:
        if c in QCODE:
            k=QCODE[c]
            if isinstance(k,tuple): key(k[0], shift=True)
            else: key(k)
        elif c.isdigit() or c.islower(): key(c)
        else: raise RuntimeError('no qcode for %r' % c)
def command(c, need=None, prompt=True):
    # Type only when the machine is AT its prompt, and take a response only once the prompt has
    # been printed AFTER the marker that names it: a slow serial line (riscv64 under load) can
    # still be printing the previous answer when the next command is typed, and a poll that read
    # that remainder as this command's answer would judge the window on stale state.
    end=time.time()+60
    while time.time()<end and not txt().rstrip().endswith('aletheia>'):
        time.sleep(.1)
    n=len(txt()); s.sendall(c.encode()+b'\r')
    while time.time()<end:
        tail=txt()[n:]
        # The answer is complete only when the prompt is the LAST thing printed: the readout
        # itself quotes the terminal's last line ("aletheia> input"), so a prompt found anywhere
        # inside the tail proves nothing about whether the answer has finished.
        if (need is None or need in tail) and (not prompt or tail.rstrip().endswith('aletheia>')):
            return tail
        time.sleep(.1)
    raise RuntimeError('serial command timed out: '+c)
# The window's navigation is collected on the console session's IDLE turn (ADR-157): a poll that
# typed `input` back to back would keep the session busy and starve the very path under test. So
# each poll is followed by a pause, and the bound is what turns "slow" into a failure.
def eventually(pred, why, secs=30):
    end=time.time()+secs; last=''
    while time.time()<end:
        last=command('input', need='session:')
        if pred(last): return last
        time.sleep(1.0)
    raise RuntimeError('the machine never reported '+why+': '+last[-400:])
# The peer serves both legs; this leg's evidence is what it logged from here on.
peer_mark=len(open(peer_log).read())
def peer_since(): return open(peer_log).read()[peer_mark:]
t=txt(); assert '[desktop] LIVE:' in t, 'no live desktop'
assert '5 managed windows' in t, 'the desktop did not report five managed windows'
# 1 - the operator pins the peer on the serial line; the window uses THIS table.
out=command('trust %s 10.0.2.2 %s' % (name, pin))
assert name in out and 'usage' not in out, 'trust did not take the host: '+out[-200:]
# 2 - Alt+5 focuses the browser window through the real keyboard path.
send([down('alt'),down('5'),up('5'),up('alt')])
eventually(lambda o: 'focus: surface 9' in o, 'the browser window taking focus after Alt+5')
# 3 - the URL is typed into the window, one real key event at a time, and the window shows it.
url='https://%s:%s/plain.txt' % (name, port)
type_text(url)
eventually(lambda o: ('browser: url "%s"' % url) in o, 'the typed URL in the window')
# 4 - Enter latches it; the console session collects it, dials the peer and pushes the page back.
key('ret')
o=eventually(lambda o: re.search(r'browser: url "%s", page \d+ bytes, first "https://%s' % (re.escape(url), re.escape(name)), o) is not None,
             'the fetched page in the browser window', secs=240)
m=re.search(r'page (\d+) bytes', o); assert m and int(m.group(1)) > 40, 'the page is too short: '+o[-300:]
peer=peer_since()
assert ('peer request: /plain.txt host: %s' % name) in peer, "the peer never saw the window's GET: "+peer[-300:]
assert 'agent: aletheia/0.1' in peer, 'the window did not send the fixed user agent'
# 5 - the window refuses what the console refuses: a plaintext URL typed here is refused by name,
#     in the window, with nothing dialed.
for _ in url: key('backspace')
eventually(lambda o: 'browser: url ""' in o, 'the URL line emptied by Backspace')
plain='http://%s:%s/plain.txt' % (name, port)
type_text(plain); key('ret')
eventually(lambda o: ('browser: url "%s", page' % plain) in o and 'first "refused: plaintext' in o,
           'the plaintext refusal in the browser window', secs=120)
peer=peer_since()
assert peer.count('peer request:') == 1, 'something was dialed for the plaintext URL: '+peer[-300:]
# 6 - the window navigates by its own keys (ADR-164): an HTML page with one link, Ctrl+1 follows
#     it (the window then shows the plain page's URL as its first line and the peer saw the GET),
#     Ctrl+B goes back (the index is fetched again), Ctrl+F forward, Ctrl+9 is refused by name.
for _ in plain: key('backspace')
eventually(lambda o: 'browser: url ""' in o, 'the URL line emptied again')
index='https://%s:%s/index.html' % (name, port)
type_text(index); key('ret')
eventually(lambda o: ('first "%s' % index[:30]) in o, 'the index page in the window', secs=120)
key('1', ctrl=True)
eventually(lambda o: ('first "%s' % url[:30]) in o, 'the followed link (plain page) in the window', secs=120)
peer=peer_since()
assert peer.count('peer request: /plain.txt') == 2, 'Ctrl+1 did not fetch the linked page: '+peer[-300:]
key('b', ctrl=True)
eventually(lambda o: ('first "%s' % index[:30]) in o, 'back to the index by Ctrl+B', secs=120)
key('f', ctrl=True)
eventually(lambda o: ('first "%s' % url[:30]) in o, 'forward to the plain page by Ctrl+F', secs=120)
key('9', ctrl=True)
eventually(lambda o: 'first "refused: the page offers no such link' in o, 'Ctrl+9 refused by name', secs=60)
command('halt', need='halting', prompt=False)
print('BROWSER LIVE E2E: PASS', flush=True)
PY
  local rc=$?
  kill -9 "$pid" 2>/dev/null || true; trap - RETURN; rm -f "$qmp" "$ser"
  if [ "$rc" -eq 0 ]; then
    echo "BROWSER-E2E-$arch: PASS"
  else
    echo "BROWSER-E2E-$arch: FAIL (rc=$rc)"; echo "--- last 40 serial lines"; tail -40 "$log" 2>/dev/null; echo "--- peer log"; tail -20 "$PEER_LOG"
    fail=1
  fi
}

run_target aarch64
run_target riscv64
if [ "$fail" -eq 0 ]; then
  echo "BROWSER-E2E: PASS — a URL typed into the desktop's browser window with a real keyboard fetched a real page over TLS and showed it there; plaintext typed there was refused there"
else
  echo "BROWSER-E2E: FAIL"; exit 1
fi
