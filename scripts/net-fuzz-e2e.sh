#!/usr/bin/env bash
# A hostile network peer, live, on all three CPUs (ADR-181).
#
# tls-e2e, https-e2e and dns-e2e prove the network against peers that behave. The hostile-page
# property campaign (ADR-161) attacks the parsers on the host. Nothing attacked the LIVE stack - the
# driver, TCP, the TLS pump, the HTTP reader, the resolver - from the other end of a real socket.
# This gate does: one TCP port and one UDP port on this host, where every connection or query draws
# a behaviour from a seeded generator:
#
#   close at once | reset (RST) | random bytes | a TLS record header promising 16 MiB | TLS-shaped
#   garbage | trickle one byte at a time | flood 256 KiB | accept and say nothing | a real TLS
#   handshake followed by garbage records | a real TLS handshake followed by a malformed HTTP head
#   (and for UDP: random bytes, a truncated header, a reply to another id, a pointer loop, silence)
#
# and a scripted operator types NET_FUZZ_COMMANDS `tcp`, `tls`, `https` and `resolve` commands at
# the peer, each followed by an `echo` sentinel. PASS on every CPU: every command answered (a
# refusal by name is an answer), nothing panicked, `mem` still answers and the heap grew by less
# than NET_FUZZ_HEAP_BOUND bytes (48 KiB + 400 B per command) across the storm, and `halt` gives the clean exit code.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi
command -v python3 >/dev/null 2>&1 || { echo "NET-FUZZ-E2E: SKIP (python3 runs the hostile peer)"; exit 0; }
python3 -c "import cryptography, ssl" 2>/dev/null || {
  echo "NET-FUZZ-E2E: SKIP (python3 needs 'cryptography' to issue the fixture certificate)"; exit 0; }

NET_FUZZ_COMMANDS="${NET_FUZZ_COMMANDS:-60}"
NET_FUZZ_SEED="${NET_FUZZ_SEED:-0x4e7f022}"
NET_FUZZ_TIMEOUT="${NET_FUZZ_TIMEOUT:-180}"
# The heap may grow by a fixed allowance (the first conversation that reaches traffic keys builds
# the 33 KB record workspace, once) plus a small per-command residual (ADR-181 measures ~0 B for
# tcp/tls/resolve and ~100-200 B on some https conversations). The leak this gate was written
# against cost 1.3-4.8 KB per TLS conversation and fails it.
NET_FUZZ_HEAP_BOUND="${NET_FUZZ_HEAP_BOUND:-$((49152 + 400 * NET_FUZZ_COMMANDS))}"
NET_FUZZ_TARGETS="${NET_FUZZ_TARGETS:-aarch64 riscv64 x86-64}"
want() { case " $NET_FUZZ_TARGETS " in *" $1 "*) return 0 ;; esac; return 1; }
SERVER_NAME="aletheia.test"

WORK="$(mktemp -d)"
PIN="$(python3 "$ROOT/scripts/tls-fixtures.py" pem "$WORK")" || { echo "NET-FUZZ-E2E: FAIL (fixture PEMs)"; exit 1; }
PEER_LOG="$WORK/peer.log"
cat > "$WORK/peer.py" <<'PYSRC'
import os, random, socket, ssl, struct, sys, threading, time

port_file, cert, key, seed = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4], 16)
rng = random.Random(seed)
lock = threading.Lock()
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.minimum_version = ssl.TLSVersion.TLSv1_3
ctx.load_cert_chain(cert, key)

tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
tcp.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
tcp.bind(("127.0.0.1", 0)); tcp.listen(16)
udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
udp.bind(("127.0.0.1", 0))
open(port_file, "w").write(f"{tcp.getsockname()[1]} {udp.getsockname()[1]}")

TCP_KINDS = ["close", "reset", "random", "huge-record", "tls-garbage", "trickle", "flood", "silent",
             "tls-then-garbage", "tls-then-bad-http"]

def pick(kinds):
    with lock:
        return rng.choice(kinds), rng.randrange(1 << 30)

def serve(raw):
    kind, r = pick(TCP_KINDS)
    lr = random.Random(r)
    print("tcp:", kind, flush=True)
    raw.settimeout(20)
    try:
        if kind == "close":
            pass
        elif kind == "reset":
            raw.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
        elif kind == "random":
            raw.sendall(bytes(lr.randrange(256) for _ in range(lr.randrange(1, 5000))))
        elif kind == "huge-record":
            raw.sendall(b"\x16\x03\x03\xff\xff" + bytes(lr.randrange(256) for _ in range(300)))
        elif kind == "tls-garbage":
            for _ in range(lr.randrange(1, 20)):
                n = lr.randrange(0, 600)
                raw.sendall(bytes([lr.choice([0x14, 0x15, 0x16, 0x17]), 3, 3]) + struct.pack(">H", n)
                            + bytes(lr.randrange(256) for _ in range(n)))
        elif kind == "trickle":
            for _ in range(8):
                raw.sendall(bytes([lr.randrange(256)])); time.sleep(0.4)
        elif kind == "flood":
            chunk = bytes(lr.randrange(256) for _ in range(4096))
            for _ in range(64):
                raw.sendall(chunk)
        elif kind == "silent":
            time.sleep(15)
        elif kind in ("tls-then-garbage", "tls-then-bad-http"):
            conn = ctx.wrap_socket(raw, server_side=True)
            try:
                conn.recv(2048)
            except Exception:
                pass
            if kind == "tls-then-garbage":
                conn.sendall(bytes(lr.randrange(256) for _ in range(lr.randrange(1, 3000))))
                raw = conn
            else:
                heads = [b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\nx",
                         b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzzzz\r\n",
                         b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\nhello",
                         b"HTTP/9.9 999\r\n" + b"X: " + b"a" * 9000 + b"\r\n\r\n",
                         b"\x00\x01\x02 not http at all",
                         b"HTTP/1.1 200 OK\r\n" + b"H: v\r\n" * 500 + b"\r\n"]
                conn.sendall(lr.choice(heads))
                raw = conn
    except Exception as exc:
        print("tcp error:", kind, exc, flush=True)
    finally:
        try: raw.close()
        except Exception: pass

def serve_udp():
    udp.settimeout(1800)
    while True:
        try:
            msg, addr = udp.recvfrom(512)
        except socket.timeout:
            return
        kind, r = pick(["random", "short", "other-id", "loop", "silent", "flood-answers"])
        lr = random.Random(r)
        print("udp:", kind, flush=True)
        if kind == "random":
            udp.sendto(bytes(lr.randrange(256) for _ in range(lr.randrange(1, 512))), addr)
        elif kind == "short":
            udp.sendto(msg[:lr.randrange(0, 12)], addr)
        elif kind == "other-id":
            udp.sendto(bytes([msg[0] ^ 0xFF, msg[1]]) + b"\x81\x80" + msg[4:], addr)
        elif kind == "loop":
            q = msg[12:]
            udp.sendto(msg[:2] + b"\x81\x80\x00\x01\x00\x01\x00\x00\x00\x00" + q + b"\xc0\x0c" * 200, addr)
        elif kind == "flood-answers":
            q = msg[12:]
            ans = b"\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x3c\x00\x04\x0a\x00\x02\x02"
            udp.sendto((msg[:2] + b"\x81\x80\x00\x01\xff\xff\x00\x00\x00\x00" + q + ans * 60)[:512], addr)

threading.Thread(target=serve_udp, daemon=True).start()
tcp.settimeout(1800)
while True:
    try:
        raw, _ = tcp.accept()
    except socket.timeout:
        break
    threading.Thread(target=serve, args=(raw,), daemon=True).start()
PYSRC
python3 "$WORK/peer.py" "$WORK/ports" "$WORK/leaf.pem" "$WORK/leaf-key.pem" "$NET_FUZZ_SEED" > "$PEER_LOG" 2>&1 &
PEER_PID=$!
trap 'kill "$PEER_PID" 2>/dev/null; rm -rf "$WORK"' EXIT
for _ in $(seq 1 50); do [ -s "$WORK/ports" ] && break; sleep 0.2; done
TCP_PORT=""; UDP_PORT=""
read -r TCP_PORT UDP_PORT < "$WORK/ports" || true  # the file has no trailing newline
[ -n "$TCP_PORT" ] && [ -n "$UDP_PORT" ] || { echo "NET-FUZZ-E2E: FAIL (the peer never opened its ports)"; exit 1; }
echo "==> hostile peer: tcp 127.0.0.1:$TCP_PORT, udp 127.0.0.1:$UDP_PORT (the guest dials 10.0.2.2), seed $NET_FUZZ_SEED"

fuzz_one() {
  local label="$1" clean_rc="$2"; shift 2
  python3 - "$label" "$NET_FUZZ_SEED" "$NET_FUZZ_COMMANDS" "$NET_FUZZ_TIMEOUT" "$clean_rc" \
    "$TCP_PORT" "$UDP_PORT" "$SERVER_NAME" "$PIN" "$NET_FUZZ_HEAP_BOUND" "$@" <<'PY'
import os, random, re, select, subprocess, sys, time
label, seed, n, limit, clean_rc, tport, uport, name, pin, bound = sys.argv[1:11]
argv = sys.argv[11:]
n, limit, clean_rc, bound = int(n), float(limit), int(clean_rc), int(bound)
rng = random.Random(int(seed, 16) ^ sum(label.encode()))
BAD = [b"panic", b"PANIC", b"FATAL", b"[FAIL", b"Kernel fault", b"unhandled"]
p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
buf = bytearray()
logf = open(os.environ["NET_FUZZ_LOG"], "wb") if os.environ.get("NET_FUZZ_LOG") else None
def pump(t):
    r, _, _ = select.select([p.stdout], [], [], t)
    if r:
        c = os.read(p.stdout.fileno(), 65536)
        if c:
            buf.extend(c)
            if logf: logf.write(c); logf.flush()
            return True
    return False
def fail(why, cmd=None):
    print(f"  FAIL [{label}] {why} (seed {seed})")
    if cmd: print(f"    the command: {cmd!r}")
    print("    last bytes: " + bytes(buf[-400:]).decode("utf-8", "replace").replace("\n", "\n    | "))
    try: p.kill()
    except Exception: pass
    sys.exit(1)
def run(cmd, i):
    mark = len(buf)
    p.stdin.write(cmd + b"\r" + b"echo NF-%d\r" % i); p.stdin.flush()
    tag = b"\nNF-%d\r" % i
    end = time.time() + limit
    while buf.find(tag, mark) < 0:
        if p.poll() is not None or time.time() > end:
            fail(f"command {i} was never answered", cmd)
        pump(0.05)
    for bad in BAD:
        if bad in buf[start:]:
            fail(f"command {i} made the machine print {bad.decode()!r}", cmd)
    return bytes(buf[mark:])
def heap(i):
    out = run(b"mem", i)
    m = re.search(rb"heap: (\d+) B used", out)
    return int(m.group(1)) if m else None
end = time.time() + 400
while b"aletheia> " not in buf:
    if time.time() > end or p.poll() is not None: fail("the console never reached its first prompt")
    pump(0.05)
time.sleep(1.5)
while pump(0.2): pass
start = len(buf)
# Warm-up, one of each kind: the first `tls` builds the console's one TLS pump (ADR-151, ~90 KB,
# kept for the machine's life), and that is a build, not a leak.
run(b"tcp 10.0.2.2 %s warm" % tport.encode(), 0)
run(b"tls 10.0.2.2 %s %s %s warm" % (tport.encode(), name.encode(), pin.encode()), 0)
run(b"https 10.0.2.2 %s %s %s /warm" % (tport.encode(), name.encode(), pin.encode()), 0)
run(b"resolve warm.aletheia.test 10.0.2.2 %s" % uport.encode(), 0)
per = {}
def cost(k, cmd, i):
    a = heap(i)
    run(cmd, i)
    b = heap(i)
    if a is not None and b is not None:
        per.setdefault(k, []).append(b - a)
h0 = heap(1)
kinds = {"tcp": 0, "tls": 0, "https": 0, "resolve": 0}
t0 = time.time()
for i in range(2, n + 2):
    k = rng.choice(list(kinds))
    kinds[k] += 1
    if k == "tcp":
        cmd = b"tcp 10.0.2.2 %s hello-%d" % (tport.encode(), i)
    elif k == "tls":
        cmd = b"tls 10.0.2.2 %s %s %s hi" % (tport.encode(), name.encode(), pin.encode())
    elif k == "https":
        cmd = b"https 10.0.2.2 %s %s %s /x" % (tport.encode(), name.encode(), pin.encode())
    else:
        cmd = b"resolve h%d.aletheia.test 10.0.2.2 %s" % (i, uport.encode())
    if os.environ.get("NET_FUZZ_PROFILE"):
        cost(k, cmd, i)
    else:
        run(cmd, i)
h1 = heap(n + 2)
if per:
    print("    heap per command: " + ", ".join(f"{k} {sum(v) / len(v):.0f} B (max {max(v)})" for k, v in per.items()))
    for k, v in per.items(): print(f"      {k}: {v}")
if h0 is not None and h1 is not None and h1 - h0 > bound:
    fail(f"the heap grew {h1 - h0} B across the storm (bound {bound})")
mark = len(buf)
p.stdin.write(b"halt\r"); p.stdin.flush()
try:
    rc = p.wait(timeout=120)
except subprocess.TimeoutExpired:
    fail("after the storm, `halt` did not end the machine")
if rc != clean_rc:
    fail(f"the machine exited {rc}, not its clean {clean_rc}")
grown = "n/a" if h0 is None or h1 is None else f"{h1 - h0} B"
print(f"    {n} commands against a hostile peer ({kinds}), all answered in {time.time() - t0:.0f} s, heap +{grown}, `halt` clean")
PY
}

net_devices_mmio=(-netdev user,id=n0 -device virtio-net-device,netdev=n0 -device virtio-rng-device)
fail=0; declare -a RESULTS=()
mmio_leg() {
  local label="$1" dir="$2" triple="$3" bin="$4"; shift 4
  echo "==> $label: building WITH the interactive console"
  local elf="$ROOT/$dir/target/$triple/debug/$bin"
  if [ "$label" = aarch64 ] && [ -n "${NET_FUZZ_ELF:-}" ]; then
    elf="$NET_FUZZ_ELF"  # a prebuilt kernel, e.g. a `heaptrace` build (ADR-182)
  else
    ( cd "$ROOT/$dir" && cargo build -q --features interactive ) || { echo "  FAIL [$label] build"; return 1; }
  fi
  local img="$ROOT/$dir/target/netfuzz-scratch.img"
  dd if=/dev/zero of="$img" bs=1048576 count=1 2>/dev/null
  fuzz_one "$label" 0 "$@" -kernel "$elf" \
    -global virtio-mmio.force-legacy=false \
    -drive "if=none,format=raw,file=$img,id=blk0" -device virtio-blk-device,drive=blk0 \
    "${net_devices_mmio[@]}"
}
x86_leg() {
  local code="" vars=""
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
    echo "  x86-64 leg unavailable (needs qemu-system-x86_64 + mtools + OVMF) — SKIPPED (never a silent pass)"
    return 2
  fi
  echo "==> x86-64: building the UEFI image WITH the interactive console"
  local img="$ROOT/kernel-x86_64/build/aletheia-x86_64-interactive.img"
  CARGO_FEATURES=interactive IMG="$img" bash "$ROOT/kernel-x86_64/scripts/build-image-linux.sh" >/dev/null \
    || { echo "  FAIL [x86-64] build"; return 1; }
  local w; w="$(mktemp -d)"; cp "$vars" "$w/vars.fd"
  dd if=/dev/zero of="$w/s.img" bs=1048576 count=1 2>/dev/null
  dd if=/dev/zero of="$w/p.img" bs=1048576 count=1 2>/dev/null
  fuzz_one "x86-64" 33 qemu-system-x86_64 -machine q35 -m 256 -smp 4 -cpu qemu64,+smep \
    -display none -serial stdio -monitor none \
    -drive "if=pflash,format=raw,unit=0,file=$code,readonly=on" \
    -drive "if=pflash,format=raw,unit=1,file=$w/vars.fd" \
    -drive "format=raw,file=$img" \
    -drive "if=none,format=raw,file=$w/s.img,id=blk0" -device virtio-blk-pci,drive=blk0 \
    -drive "if=none,format=raw,file=$w/p.img,id=blk1" -device virtio-blk-pci,drive=blk1 \
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0 -device virtio-rng-pci,disable-legacy=on \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 -no-reboot
  local rc=$?; rm -rf "$w"; return $rc
}

if want aarch64; then
  mmio_leg "aarch64" kernel aarch64-unknown-none-softfloat aletheia-kernel \
    qemu-system-aarch64 -machine virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 128M \
    -display none -serial stdio -monitor none -semihosting-config enable=on,target=native
  rc=$?; [ "$rc" -eq 0 ] || fail=1; RESULTS+=("aarch64 : $([ "$rc" -eq 0 ] && echo PASS || echo FAIL)")
fi
if want riscv64; then
  mmio_leg "riscv64" kernel-riscv64 riscv64gc-unknown-none-elf aletheia-kernel-riscv64 \
    qemu-system-riscv64 -machine virt -cpu rv64 -smp 4 -m 128M -display none -serial stdio -monitor none -bios default
  rc=$?; [ "$rc" -eq 0 ] || fail=1; RESULTS+=("riscv64 : $([ "$rc" -eq 0 ] && echo PASS || echo FAIL)")
fi
if want x86-64; then x86_leg; rc=$?; else rc=2; fi
case "$rc" in 0) RESULTS+=("x86-64  : PASS") ;; 2) RESULTS+=("x86-64  : SKIP") ;; *) fail=1; RESULTS+=("x86-64  : FAIL") ;; esac

echo "peer behaviours drawn: $(grep -c '^tcp:' "$PEER_LOG") tcp, $(grep -c '^udp:' "$PEER_LOG") udp"
for r in "${RESULTS[@]}"; do echo "  $r"; done
if [ "$fail" -eq 0 ]; then
  echo "NET-FUZZ-E2E: PASS — every command against a hostile peer was answered, nothing panicked, the heap held, the machine halts cleanly"
  exit 0
fi
echo "NET-FUZZ-E2E: FAIL"
exit 1
