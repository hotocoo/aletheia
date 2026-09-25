#!/usr/bin/env bash
# Pull the plug, many times, on all three CPUs (ADR-183).
#
# The journal's crash consistency is proved on the host by injecting a crash at every recorded
# device operation, and the persistence gates reboot cleanly and read back. Nothing killed a RUNNING
# machine mid-write and looked at what the real virtio-blk device kept. This gate does: on one
# persistent disk image, CRASH_ROUNDS times, it boots the interactive console, checks that the
# namespace mounts and that every object holds exactly ONE complete version (never a torn mixture,
# never a version that was never written), then types a stream of whole-object rewrites and
# removals and SIGKILLs QEMU at a random moment - mid-command, mid-flush, whenever.
#
# Objects are o0..o7; version v of object k reads `v<v>-k<k>-` followed by filler whose length also
# encodes v, so a torn object cannot pass for any real version. An object that was rewritten or
# removed in the crashed round may show either its old or its new state; anything else fails.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi
command -v python3 >/dev/null 2>&1 || { echo "CRASH-E2E: SKIP (python3 drives the rounds)"; exit 0; }
CRASH_ROUNDS="${CRASH_ROUNDS:-12}"
CRASH_SEED="${CRASH_SEED:-0xc4a54}"
CRASH_TARGETS="${CRASH_TARGETS:-aarch64 riscv64 x86-64}"
want() { case " $CRASH_TARGETS " in *" $1 "*) return 0 ;; esac; return 1; }
fail=0; declare -a RESULTS=()

rounds() {
  local label="$1"; shift
  python3 - "$label" "$CRASH_SEED" "$CRASH_ROUNDS" "$@" <<'PY'
import os, random, re, select, signal, subprocess, sys, time
label, seed, rounds = sys.argv[1:4]
argv = sys.argv[4:]
rounds = int(rounds)
rng = random.Random(int(seed, 16) ^ sum(label.encode()))
N = 8
def body(k, v):
    return f"v{v}-k{k}-" + "x" * (20 + (v * 37 + k * 11) % 180)
def parse(text):
    m = re.fullmatch(r"v(\d+)-k(\d+)-(x*)", text)
    if not m: return None
    v, k = int(m.group(1)), int(m.group(2))
    return (k, v) if body(k, v) == text else None
# committed[k] = set of states the object may be in: an int version, or None (absent)
committed = {k: {None} for k in range(N)}
version = 0

def boot():
    p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    return p, bytearray()
def pump(p, buf, t):
    r, _, _ = select.select([p.stdout], [], [], t)
    if r:
        c = os.read(p.stdout.fileno(), 65536)
        if c: buf.extend(c); return True
    return False
def wait_prompt(p, buf, mark, limit):
    end = time.time() + limit
    while buf.find(b"aletheia> ", mark) < 0:
        if p.poll() is not None or time.time() > end: return False
        pump(p, buf, 0.05)
    return True
def run(p, buf, cmd, limit=60):
    mark = len(buf)
    p.stdin.write(cmd + b"\r"); p.stdin.flush()
    end = time.time() + limit
    while True:
        i = buf.find(b"aletheia> ", mark + len(cmd))
        if i >= 0: return bytes(buf[mark:i])
        if p.poll() is not None or time.time() > end: return None
        pump(p, buf, 0.05)
def fail(why, buf=b""):
    print(f"  FAIL [{label}] {why} (seed {seed})")
    print("    last bytes: " + bytes(buf[-500:]).decode("utf-8", "replace").replace("\n", "\n    | "))
    sys.exit(1)

kills = 0; checked = 0; present = 0; t0 = time.time()
for rnd in range(rounds + 1):
    p, buf = boot()
    if not wait_prompt(p, buf, 0, 400):
        fail(f"round {rnd}: the machine never reached its prompt after the crash", buf)
    if b"no usable namespace" in buf or b"FATAL" in buf:
        fail(f"round {rnd}: the namespace did not mount after the crash", buf)
    time.sleep(1)
    while pump(p, buf, 0.2): pass
    # Verify every object against what may legally be on the disk.
    listing = run(p, buf, b"ls")
    if listing is None: fail(f"round {rnd}: `ls` did not answer", buf)
    now = {}
    for k in range(N):
        out = run(p, buf, b"cat o%d" % k)
        if out is None: fail(f"round {rnd}: `cat o{k}` did not answer", buf)
        text = out.decode("utf-8", "replace").split("\r\n", 1)[-1].strip()
        if "no such object" in text or text == "":
            state = None
        else:
            got = parse(text.splitlines()[0].strip() if text else "")
            if got is None or got[0] != k:
                fail(f"round {rnd}: o{k} holds a TORN or foreign value: {text[:120]!r}", buf)
            state = got[1]
        if state not in committed[k]:
            fail(f"round {rnd}: o{k} is {state!r}, but only {sorted(committed[k], key=str)} were legal", buf)
        now[k] = state
        checked += 1
        present += state is not None
    committed = {k: {now[k]} for k in range(N)}
    if rnd == rounds:
        p.stdin.write(b"halt\r"); p.stdin.flush()
        try: p.wait(timeout=60)
        except subprocess.TimeoutExpired: p.kill()
        break
    # A stream of mutations, killed at a random moment. Every issued mutation widens the set of
    # legal states for its object until its answer is seen.
    deadline = time.time() + rng.uniform(0.2, 6.0)
    killed = False
    while not killed:
        k = rng.randrange(N)
        if rng.randrange(5) == 0:
            cmd, new = b"rm o%d" % k, None
        else:
            version += 1
            cmd, new = b"write o%d %s" % (k, body(k, version).encode()), version
        committed[k].add(new)
        mark = len(buf)
        p.stdin.write(cmd + b"\r"); p.stdin.flush()
        while buf.find(b"aletheia> ", mark + len(cmd)) < 0:
            if time.time() > deadline:
                p.send_signal(signal.SIGKILL); p.wait(); killed = True; kills += 1
                break
            if p.poll() is not None: fail(f"round {rnd}: the machine died on its own", buf)
            pump(p, buf, 0.01)
        else:
            # Answered: the object is now exactly the new state (or unchanged, if refused).
            out = bytes(buf[mark:]).decode("utf-8", "replace")
            if ("wrote" in out or "removed" in out):
                committed[k] = {new}
            else:
                committed[k].discard(new) if len(committed[k]) > 1 else None
print(f"    {kills} crashes, {version} versions issued, {checked} object checks ({present} found present), every object whole and legal after every crash ({time.time() - t0:.0f} s)")
if present == 0:
    fail("no object was ever found present - the check proved nothing")
PY
}

mmio_leg() {
  local label="$1" dir="$2" triple="$3" bin="$4"; shift 4
  echo "==> $label: building WITH the interactive console"
  ( cd "$ROOT/$dir" && cargo build -q --features interactive ) || { echo "  FAIL [$label] build"; return 1; }
  local s="$ROOT/$dir/target/crash-scratch.img" d="$ROOT/$dir/target/crash-persist.img"
  dd if=/dev/zero of="$s" bs=1048576 count=1 2>/dev/null
  dd if=/dev/zero of="$d" bs=1048576 count=1 2>/dev/null
  rounds "$label" "$@" -kernel "$ROOT/$dir/target/$triple/debug/$bin" \
    -global virtio-mmio.force-legacy=false \
    -drive "if=none,format=raw,file=$s,id=blk0" -device virtio-blk-device,drive=blk0 \
    -drive "if=none,format=raw,file=$d,id=blk1" -device virtio-blk-device,drive=blk1
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
    echo "  x86-64 leg unavailable (needs qemu-system-x86_64 + mtools + OVMF) — SKIPPED (never a silent pass)"; return 2
  fi
  echo "==> x86-64: building the UEFI image WITH the interactive console"
  local img="$ROOT/kernel-x86_64/build/aletheia-x86_64-interactive.img"
  CARGO_FEATURES=interactive IMG="$img" bash "$ROOT/kernel-x86_64/scripts/build-image-linux.sh" >/dev/null \
    || { echo "  FAIL [x86-64] build"; return 1; }
  local w; w="$(mktemp -d)"; cp "$vars" "$w/vars.fd"
  dd if=/dev/zero of="$w/s.img" bs=1048576 count=1 2>/dev/null
  dd if=/dev/zero of="$w/p.img" bs=1048576 count=1 2>/dev/null
  rounds "x86-64" qemu-system-x86_64 -machine q35 -m 256 -smp 4 -cpu qemu64,+smep \
    -display none -serial stdio -monitor none \
    -drive "if=pflash,format=raw,unit=0,file=$code,readonly=on" \
    -drive "if=pflash,format=raw,unit=1,file=$w/vars.fd" \
    -drive "format=raw,file=$img" \
    -drive "if=none,format=raw,file=$w/s.img,id=blk0" -device virtio-blk-pci,drive=blk0 \
    -drive "if=none,format=raw,file=$w/p.img,id=blk1" -device virtio-blk-pci,drive=blk1 \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 -no-reboot
  local rc=$?; rm -rf "$w"; return $rc
}

if want aarch64; then
  mmio_leg aarch64 kernel aarch64-unknown-none-softfloat aletheia-kernel \
    qemu-system-aarch64 -machine virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 128M \
    -display none -serial stdio -monitor none -semihosting-config enable=on,target=native
  rc=$?; [ "$rc" -eq 0 ] || fail=1; RESULTS+=("aarch64 : $([ "$rc" -eq 0 ] && echo PASS || echo FAIL)")
fi
if want riscv64; then
  mmio_leg riscv64 kernel-riscv64 riscv64gc-unknown-none-elf aletheia-kernel-riscv64 \
    qemu-system-riscv64 -machine virt -cpu rv64 -smp 4 -m 128M -display none -serial stdio -monitor none -bios default
  rc=$?; [ "$rc" -eq 0 ] || fail=1; RESULTS+=("riscv64 : $([ "$rc" -eq 0 ] && echo PASS || echo FAIL)")
fi
if want x86-64; then x86_leg; rc=$?; else rc=2; fi
case "$rc" in 0) RESULTS+=("x86-64  : PASS") ;; 2) RESULTS+=("x86-64  : SKIP") ;; *) fail=1; RESULTS+=("x86-64  : FAIL") ;; esac
for r in "${RESULTS[@]}"; do echo "  $r"; done
if [ "$fail" -eq 0 ]; then
  echo "CRASH-E2E: PASS — after every pulled plug the namespace mounted and every object held one whole, legal version"
  exit 0
fi
echo "CRASH-E2E: FAIL"
exit 1
