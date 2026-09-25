#!/usr/bin/env bash
# The live console under a generated storm of hostile input, on all three CPUs (ADR-180).
#
# console-e2e.sh types a script a person would type. shellstorm (ADR-089) drives the dispatcher at
# boot, inside the kernel, with inputs the kernel itself generates. Neither types HOSTILE bytes at
# the running machine through its real serial path. This gate does: a seeded generator produces
# FUZZ_LINES lines - every console command with random, empty, huge, negative and non-ASCII
# arguments; raw control bytes and escape sequences; bytes above 0x7f; lines far past the editor's
# 256-byte bound; writes and appends until the scratch filesystem is full - and types each one into
# the booted OS, waiting for the prompt to come back.
#
# PASS means, on every CPU: after EVERY line the console answers a sentinel `echo` (no hang,
# bounded by FUZZ_LINE_TIMEOUT), nothing panicked or faulted, `ver` still answers afterwards, and `halt`
# still ends the machine with its clean exit code. The slowest line is reported. A failure prints
# the seed, the line number and the exact bytes, so it reproduces.
#
#   FUZZ_LINES=3000 FUZZ_SEED=0xa1e7 ./scripts/console-fuzz-e2e.sh
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi
command -v python3 >/dev/null 2>&1 || { echo "CONSOLE-FUZZ-E2E: SKIP (python3 drives the storm)"; exit 0; }

FUZZ_LINES="${FUZZ_LINES:-1500}"
FUZZ_SEED="${FUZZ_SEED:-0xa1e7f022}"
FUZZ_LINE_TIMEOUT="${FUZZ_LINE_TIMEOUT:-60}"
FUZZ_TARGETS="${FUZZ_TARGETS:-aarch64 riscv64 x86-64}"  # a subset reproduces one CPU's failure faster
want() { case " $FUZZ_TARGETS " in *" $1 "*) return 0 ;; esac; return 1; }
fail=0
declare -a RESULTS=()

# The command names, read from the kernel's own table so a new command is fuzzed the day it lands.
COMMANDS="$(python3 - "$ROOT/kernel-core/src/shell.rs" <<'PY'
import re, sys
src = open(sys.argv[1]).read()
table = src[src.index("pub const COMMANDS"):]
table = table[:table.index("];")]
names = sorted({m.split()[0] for m in re.findall(r'\(\s*"([^"]+)"\s*,', table)})
# halt and reboot end the session: typed once, at the end, on purpose.
print(" ".join(n for n in names if n not in ("halt", "reboot")))
PY
)"

fuzz_one() {
  local label="$1" clean_rc="$2"; shift 2
  local log; log="$(mktemp)"
  python3 - "$label" "$FUZZ_SEED" "$FUZZ_LINES" "$FUZZ_LINE_TIMEOUT" "$clean_rc" "$log" "$COMMANDS" "$@" <<'PY'
import os, random, select, subprocess, sys, time

label, seed, lines, per_line, clean_rc, log_path, commands = sys.argv[1:8]
argv = sys.argv[8:]
rng = random.Random(int(seed, 16) ^ sum(label.encode()))
lines, per_line, clean_rc = int(lines), float(per_line), int(clean_rc)
commands = commands.split()
PROMPT = b"aletheia> "
BAD = [b"panic", b"PANIC", b"FATAL", b"[FAIL", b"TERMINATED", b"Kernel fault", b"unhandled"]

def word():
    k = rng.randrange(12)
    if k == 0: return ""
    if k == 1: return str(rng.randrange(-10**12, 10**12))
    if k == 2: return "9" * rng.randrange(1, 40)
    if k == 3: return "0x" + "".join(rng.choice("0123456789abcdefg") for _ in range(rng.randrange(1, 70)))
    if k == 4: return ".".join(str(rng.randrange(0, 400)) for _ in range(rng.randrange(1, 6)))
    if k == 5: return "/" + "/".join("x" * rng.randrange(0, 9) for _ in range(rng.randrange(0, 5)))
    if k == 6: return "https://" + "".join(rng.choice("ab.-:/?#%") for _ in range(rng.randrange(0, 60)))
    if k == 7: return rng.choice(["manifesto", "poem", "a", "..", ".", "-", "--", "*", "$HOME", "%s%n"])
    return "".join(chr(rng.randrange(0x21, 0x7f)) for _ in range(rng.randrange(1, 24)))

def line():
    k = rng.randrange(10)
    if k <= 4:  # a real command with hostile arguments
        return (rng.choice(commands) + " " + " ".join(word() for _ in range(rng.randrange(0, 6)))).encode()
    if k == 5:  # fill the filesystem
        verb = rng.choice(["write", "append"])
        return f"{verb} f{rng.randrange(40)} {'x' * rng.randrange(1, 240)}".encode()
    if k == 6:  # raw control bytes and escape sequences, never CR or LF (those end the line)
        pool = [b for b in range(0x20) if b not in (0x0a, 0x0d)] + [0x7f]
        body = bytes(rng.choice(pool) for _ in range(rng.randrange(1, 30)))
        return body + b"\x1b[2J\x1b[31m" * rng.randrange(0, 3)
    if k == 7:  # bytes above 0x7f
        return bytes(rng.randrange(0x80, 0x100) for _ in range(rng.randrange(1, 40)))
    if k == 8:  # far past the editor's 256-byte bound
        return bytes(rng.randrange(0x20, 0x7f) for _ in range(rng.randrange(257, 2000)))
    return b""  # an empty line

out = open(log_path, "wb")
p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
buf = bytearray()

def pump(timeout):
    r, _, _ = select.select([p.stdout], [], [], timeout)
    if r:
        chunk = os.read(p.stdout.fileno(), 65536)
        if chunk:
            buf.extend(chunk); out.write(chunk); out.flush()
            return True
    return False

def wait_for(count, needle, limit):
    end = time.time() + limit
    while buf.count(needle) < count:
        if p.poll() is not None or time.time() > end:
            return False
        pump(0.05)
    return True

def fail(why, data=None):
    print(f"  FAIL [{label}] {why} (seed {seed})")
    if data is not None:
        print(f"    the line: {data!r}")
    print("    last bytes: " + bytes(buf[-300:]).decode("utf-8", "replace").replace("\n", "\n    | "))
    try: p.kill()
    except Exception: pass
    sys.exit(1)

if not wait_for(1, PROMPT, 400):
    fail("the console never reached its first prompt")
time.sleep(1.5)  # the banner can still be arriving after the first prompt
while pump(0.2):
    pass
start = len(buf)  # the boot log legitimately says TERMINATED (the contained-fault suites)
slowest, slowest_line, t_all = 0.0, b"", time.time()
# Synchronisation is a SENTINEL, not a prompt count: an ambiguous Tab completion legitimately
# reprints the prompt, so counting prompts ran ahead of the machine and read answers too early.
# After each hostile line the driver types `echo FZ-<n>`; the line is finished when `FZ-<n>`
# comes back as OUTPUT (at the start of a line - the typed echo has `echo ` in front of it).
def sentinel(n, limit):
    mark = b"\nFZ-%d\r" % n
    p.stdin.write(b"echo FZ-%d\r" % n); p.stdin.flush()
    end = time.time() + limit
    while mark not in buf:
        if p.poll() is not None or time.time() > end:
            return False
        pump(0.05)
    return True

for i in range(1, lines + 1):
    data = line()
    t0 = time.time()
    mark = len(buf)
    p.stdin.write(data + b"\r"); p.stdin.flush()
    # The sentinel waits for the hostile line's own prompt: typed into the same burst, it could
    # overflow the input ring behind an over-long line on a slow host, and a ring that drops the
    # newest bytes is doing exactly what ADR-045 says (the first CI run, seed 0x164, lost it so).
    end = time.time() + per_line
    while buf.find(PROMPT, mark) < 0:
        if p.poll() is not None or time.time() > end:
            fail(f"line {i} of {lines} never got its prompt back", data)
        pump(0.02)
    if not sentinel(i, per_line):
        fail(f"line {i} of {lines}: the console never answered the sentinel after it", data)
    dt = time.time() - t0
    if dt > slowest:
        slowest, slowest_line = dt, data
    for bad in BAD:
        if bad in buf[start:]:
            fail(f"line {i} made the machine print {bad.decode()!r}", data)
elapsed = time.time() - t_all
mark = len(buf)
p.stdin.write(b"ver\r"); p.stdin.flush()
end = time.time() + per_line
while b"Aletheia" not in buf[mark:] and time.time() < end and p.poll() is None:
    pump(0.05)
if b"Aletheia" not in buf[mark:]:
    fail("after the storm, `ver` no longer answered")
p.stdin.write(b"halt\r"); p.stdin.flush()
try:
    rc = p.wait(timeout=120)
except subprocess.TimeoutExpired:
    fail("after the storm, `halt` did not end the machine")
while pump(0.05):
    pass
if rc != clean_rc:
    fail(f"the machine exited {rc}, not its clean {clean_rc}")
print(f"    {lines} hostile lines, every one answered, {elapsed / lines * 1000:.1f} ms/line, "
      f"slowest {slowest * 1000:.0f} ms ({slowest_line[:40]!r}), `ver` and `halt` still answer")
PY
  local rc=$?
  rm -f "$log"
  return $rc
}

mmio_leg() {
  local label="$1" dir="$2" triple="$3" bin="$4"; shift 4
  echo "==> $label: building WITH the interactive console"
  ( cd "$ROOT/$dir" && cargo build -q --features interactive ) || { echo "  FAIL [$label] build"; return 1; }
  local img="$ROOT/$dir/target/fuzz-scratch.img" pimg="$ROOT/$dir/target/fuzz-persistent.img"
  dd if=/dev/zero of="$img" bs=1048576 count=1 2>/dev/null
  dd if=/dev/zero of="$pimg" bs=1048576 count=1 2>/dev/null
  fuzz_one "$label" 0 "$@" -kernel "$ROOT/$dir/target/$triple/debug/$bin" \
    -global virtio-mmio.force-legacy=false \
    -drive "if=none,format=raw,file=$img,id=blk0" -device virtio-blk-device,drive=blk0 \
    -drive "if=none,format=raw,file=$pimg,id=blk1" -device virtio-blk-device,drive=blk1 \
    -device virtio-rng-device
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
  local work; work="$(mktemp -d)"
  cp "$vars" "$work/vars.fd"
  dd if=/dev/zero of="$work/s.img" bs=1048576 count=1 2>/dev/null
  dd if=/dev/zero of="$work/p.img" bs=1048576 count=1 2>/dev/null
  fuzz_one "x86-64" 33 qemu-system-x86_64 -machine q35 -m 256 -smp 4 -cpu qemu64,+smep -display none -serial stdio -monitor none \
    -drive "if=pflash,format=raw,unit=0,file=$code,readonly=on" \
    -drive "if=pflash,format=raw,unit=1,file=$work/vars.fd" \
    -drive "format=raw,file=$img" \
    -drive "if=none,format=raw,file=$work/s.img,id=blk0" -device virtio-blk-pci,drive=blk0 \
    -drive "if=none,format=raw,file=$work/p.img,id=blk1" -device virtio-blk-pci,drive=blk1 \
    -device virtio-rng-pci,disable-legacy=on \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 -no-reboot
  local rc=$?
  rm -rf "$work"
  return $rc
}

# `-display none -serial stdio -monitor none`, never `-nographic`: -nographic multiplexes QEMU's
# monitor onto stdio behind Ctrl-A, so a fuzzed 0x01 followed by CR was eaten by QEMU before the
# guest saw it (the first runs failed at every line ending in 0x01, on all three CPUs).
echo "==> fuzzing the live console: $FUZZ_LINES lines per CPU, seed $FUZZ_SEED"
if want aarch64; then
  mmio_leg "aarch64" kernel aarch64-unknown-none-softfloat aletheia-kernel \
    qemu-system-aarch64 -machine virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 128M -display none -serial stdio -monitor none \
    -semihosting-config enable=on,target=native
  rc=$?; [ "$rc" -eq 0 ] || fail=1; RESULTS+=("aarch64 : $([ "$rc" -eq 0 ] && echo PASS || echo FAIL)")
fi
if want riscv64; then
  mmio_leg "riscv64" kernel-riscv64 riscv64gc-unknown-none-elf aletheia-kernel-riscv64 \
    qemu-system-riscv64 -machine virt -cpu rv64 -smp 4 -m 128M -display none -serial stdio -monitor none -bios default
  rc=$?; [ "$rc" -eq 0 ] || fail=1; RESULTS+=("riscv64 : $([ "$rc" -eq 0 ] && echo PASS || echo FAIL)")
fi
if want x86-64; then x86_leg; rc=$?; else rc=2; fi
case "$rc" in 0) RESULTS+=("x86-64  : PASS") ;; 2) RESULTS+=("x86-64  : SKIP") ;; *) fail=1; RESULTS+=("x86-64  : FAIL") ;; esac

for r in "${RESULTS[@]}"; do echo "  $r"; done
if [ "$fail" -eq 0 ]; then
  echo "CONSOLE-FUZZ-E2E: PASS — every hostile line got its prompt back, nothing panicked, the machine still answers and halts cleanly"
  exit 0
fi
echo "CONSOLE-FUZZ-E2E: FAIL"
exit 1
