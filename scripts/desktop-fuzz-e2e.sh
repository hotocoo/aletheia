#!/usr/bin/env bash
# The live desktop under a storm of random input, on all three CPUs (ADR-182).
#
# The desktop gates drive the virtio keyboard and tablet with scripted, sensible events, and the
# boot storms (ADR-086/wmstorm) drive the window manager inside the kernel. Nothing threw RANDOM
# device events at the running desktop: chords with Ctrl/Alt/Shift/Meta, function keys, every
# letter and digit, Enter, Tab, Escape, arrows; absolute pointer moves anywhere including the four
# edges and corners; left/right/middle presses and releases, drags, double clicks; wheel turns.
# This gate sends DFUZZ_EVENTS of them through QMP, in batches, and after every batch requires the
# serial console to answer a sentinel (`echo DF-<n>`) and nothing to have panicked. At the end
# `input` must still answer with its window count and `halt` must end the machine.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi
command -v python3 >/dev/null 2>&1 || { echo "DESKTOP-FUZZ-E2E: SKIP (python3 drives QMP)"; exit 0; }
DFUZZ_EVENTS="${DFUZZ_EVENTS:-6000}"
DFUZZ_SEED="${DFUZZ_SEED:-0xdf022}"
DFUZZ_TARGETS="${DFUZZ_TARGETS:-aarch64 riscv64 x86_64}"
want() { case " $DFUZZ_TARGETS " in *" $1 "*) return 0 ;; esac; return 1; }
fail=0

run_target() {
  local arch="$1" dir qemu machine cpu target elf img qmp ser log rootbin dtbraw dtb
  local -a args=()
  if [ "$arch" != x86_64 ]; then
  if [ "$arch" = aarch64 ]; then
    dir="$ROOT/kernel"; qemu=qemu-system-aarch64; target=aarch64-unknown-none-softfloat
    machine='virt,iommu=smmuv3,highmem-ecam=off,gic-version=2'; cpu=cortex-a72
    elf="$dir/target/$target/debug/aletheia-kernel"
  else
    dir="$ROOT/kernel-riscv64"; qemu=qemu-system-riscv64; target=riscv64gc-unknown-none-elf
    machine=virt; cpu=rv64; elf="$dir/target/$target/debug/aletheia-kernel-riscv64"
  fi
  command -v "$qemu" >/dev/null 2>&1 || { echo "DESKTOP-FUZZ-E2E-$arch: SKIP (missing $qemu)"; return 0; }
  echo "==> [$arch] build interactive kernel"
  if [ -n "${DFUZZ_ELF:-}" ]; then
    elf="$DFUZZ_ELF"  # a prebuilt kernel, e.g. a `heaptrace` build (ADR-182)
  else
    (cd "$dir" && cargo build --features interactive) || { echo "DESKTOP-FUZZ-E2E-$arch: FAIL (build)"; fail=1; return 1; }
  fi
  img="$dir/target/desktop-fuzz.img"
  dd if=/dev/zero of="$img" bs=1048576 count=1 2>/dev/null
  # The aarch64 boot suite proves the SMMUv3 rung against a virtio-blk-pci function behind the
  # unit (ADR-074); the machine must carry one or that suite fails before the console starts.
  local pciimg="$dir/target/desktop-fuzz-pci.img"
  dd if=/dev/zero of="$pciimg" bs=1048576 count=1 2>/dev/null
  qmp="${TMPDIR:-/tmp}/aletheia-dfuzz-$arch-$$.qmp"; ser="${TMPDIR:-/tmp}/aletheia-dfuzz-$arch-$$.ser"
  log="$dir/target/desktop-fuzz-$arch.log"; rm -f "$qmp" "$ser" "$log"
  rootbin="$dir/target/capvault-root.bin"
  printf 'aletheia-capvault-root-0123456789abcdef' | head -c 32 > "$rootbin"
  # The desktop's devices AND the network's, on one machine: the window path needs both.
  local -a devices=(-device virtio-gpu-device -device virtio-keyboard-device -device virtio-tablet-device
    -drive "if=none,format=raw,file=$pciimg,id=pciblk0" -device virtio-blk-pci,disable-legacy=on,drive=pciblk0
    -netdev user,id=n0 -device virtio-net-device,netdev=n0 -device virtio-rng-device)
  args=("$qemu" -machine "$machine" -cpu "$cpu" -smp 4 -m 128M -display none -S
    -global virtio-mmio.force-legacy=false
    -drive "if=none,format=raw,file=$img,id=blk0" -device virtio-blk-device,drive=blk0
    "${devices[@]}"
    -kernel "$elf" -chardev "socket,id=ser0,path=$ser,server=on,wait=off" -serial chardev:ser0
    -qmp "unix:$qmp,server,nowait")
  if [ "$arch" = aarch64 ]; then
    dtbraw="$dir/target/desktop-fuzz-dtb-raw.bin"; dtb="$dir/target/desktop-fuzz-dtb.bin"
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

  fi
  if [ "$arch" = x86_64 ]; then
    # x86-64: a UEFI disk image under OVMF, every device over PCI - the same window path, the same
    # driver below, on the third CPU (until 2026-09-25 x86-64 only had the console's path gated).
    local code="" vars=""
    for c in "${OVMF_CODE:-}" /opt/homebrew/share/qemu/edk2-x86_64-code.fd \
        /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd /usr/share/edk2/x64/OVMF_CODE.4m.fd; do
      [ -n "$c" ] && [ -f "$c" ] && { code="$c"; break; }
    done
    for v in "${OVMF_VARS:-}" /opt/homebrew/share/qemu/edk2-i386-vars.fd \
        /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd /usr/share/edk2/x64/OVMF_VARS.4m.fd; do
      [ -n "$v" ] && [ -f "$v" ] && { vars="$v"; break; }
    done
    if ! command -v qemu-system-x86_64 >/dev/null 2>&1 || [ -z "$code" ] || [ -z "$vars" ]; then
      echo "DESKTOP-FUZZ-E2E-$arch: SKIP (needs qemu-system-x86_64 + OVMF; this leg did NOT run)"; return 0
    fi
    dir="$ROOT/kernel-x86_64"
    echo "==> [$arch] build interactive UEFI image"
    (cd "$dir" && cargo build --release --features interactive) || { echo "DESKTOP-FUZZ-E2E-$arch: FAIL (build)"; fail=1; return 1; }
    img="$dir/build/desktop-fuzz.img"; mkdir -p "$dir/build"
    python3 "$dir/scripts/mkesp.py" --efi "$dir/target/x86_64-unknown-uefi/release/aletheia-kernel-x86_64.efi" \
      --out "$img" >/dev/null || { echo "DESKTOP-FUZZ-E2E-$arch: FAIL (image)"; fail=1; return 1; }
    local work; work="$(mktemp -d)"; cp "$vars" "$work/vars.fd"
    dd if=/dev/zero of="$work/scratch.img" bs=1048576 count=1 2>/dev/null
    qmp="${TMPDIR:-/tmp}/aletheia-dfuzz-$arch-$$.qmp"; ser="${TMPDIR:-/tmp}/aletheia-dfuzz-$arch-$$.ser"
    log="$dir/build/desktop-fuzz-$arch.log"; rm -f "$qmp" "$ser" "$log"
    args=(qemu-system-x86_64 -machine q35 -m 256 -smp 4 -cpu qemu64,+smep -display none -S
      -drive "if=pflash,format=raw,unit=0,readonly=on,file=$code"
      -drive "if=pflash,format=raw,unit=1,file=$work/vars.fd"
      -drive "format=raw,file=$img"
      -drive "if=none,format=raw,file=$work/scratch.img,id=blk0" -device virtio-blk-pci,drive=blk0
      -device virtio-gpu-pci,disable-legacy=on -device virtio-keyboard-pci -device virtio-tablet-pci
      -netdev user,id=n0 -device virtio-net-pci,netdev=n0 -device virtio-rng-pci,disable-legacy=on
      -chardev "socket,id=ser0,path=$ser,server=on,wait=off" -serial chardev:ser0
      -qmp "unix:$qmp,server,nowait")
  fi

  "${args[@]}" & local pid=$!
  trap 'kill -9 "$pid" 2>/dev/null || true; rm -f "$qmp" "$ser"' RETURN

  python3 - "$qmp" "$ser" "$log" "$arch" "$DFUZZ_SEED" "$DFUZZ_EVENTS" <<'PY'
import json, random, socket, sys, threading, time
qmp, serial, log, arch, seed, total = sys.argv[1:]
total = int(total)
import os
DFUZZ_HEAP_BOUND = int(os.environ.get('DFUZZ_HEAP_BOUND', '16384'))
rng = random.Random(int(seed, 16) ^ sum(arch.encode()))
def conn(path):
    end = time.time() + 60
    while time.time() < end:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try: s.connect(path); return s
        except OSError: s.close(); time.sleep(.1)
    raise RuntimeError('socket timeout: ' + path)
q = conn(qmp); qf = q.makefile('rwb'); qf.readline()
def qcmd(x):
    qf.write((json.dumps(x) + '\n').encode()); qf.flush()
    while True:
        m = json.loads(qf.readline())
        if 'return' in m or 'error' in m: return m
s = conn(serial); lf = open(log, 'ab')
def rd():
    while True:
        try: b = s.recv(4096)
        except OSError: return
        if not b: return
        lf.write(b); lf.flush()
threading.Thread(target=rd, daemon=True).start()
def txt():
    try: return open(log, 'rb').read().decode('utf-8', 'replace')
    except FileNotFoundError: return ''
qcmd({'execute': 'qmp_capabilities'}); qcmd({'execute': 'cont'})
end = time.time() + 300
while 'aletheia>' not in txt():
    if time.time() > end: raise SystemExit('FAIL: the interactive console never started')
    time.sleep(.2)
time.sleep(2)
start = len(txt())
import re
def heap():
    mark = len(txt()); s.sendall(b'\rmem\r'); end = time.time() + 30
    while time.time() < end:
        m = re.search(r'heap: (\d+) B used', txt()[mark:])
        if m: return int(m.group(1))
        time.sleep(.05)
    return None
h0 = heap()
if os.environ.get('DFUZZ_IDLE'):
    time.sleep(float(os.environ['DFUZZ_IDLE']))
    hi = heap()
    print('    idle %ss: heap +%d B' % (os.environ['DFUZZ_IDLE'], hi - h0))
    h0 = hi
BAD = ['panic', 'PANIC', 'FATAL', '[FAIL', 'Kernel fault', 'unhandled']
MODS = ['shift', 'ctrl', 'alt', 'meta_l']
KEYS = list('abcdefghijklmnopqrstuvwxyz0123456789') + ['ret', 'tab', 'esc', 'spc', 'backspace',
        'up', 'down', 'left', 'right', 'home', 'end', 'pgup', 'pgdn', 'delete', 'insert',
        'minus', 'equal', 'slash', 'dot', 'comma', 'semicolon', 'apostrophe', 'bracket_left',
        'bracket_right', 'grave_accent', 'backslash'] + ['f%d' % i for i in range(1, 13)]
def ev_key(k, down): return {'type': 'key', 'data': {'down': down, 'key': {'type': 'qcode', 'data': k}}}
def ev_abs(axis, v): return {'type': 'abs', 'data': {'axis': axis, 'value': v}}
def ev_btn(b, down): return {'type': 'btn', 'data': {'button': b, 'down': down}}
def coord():
    k = rng.randrange(5)
    if k == 0: return rng.choice([0, 32767])            # an edge
    if k == 1: return rng.choice([0, 1, 32766, 32767])  # a corner-ish value
    return rng.randrange(32768)
# Hot spots on the 640x240 scanout (desktop.rs geometry): each window's title band and the right
# end where its close/max/min boxes sit, and the taskbar row. Half of all aimed pointer events land
# on one, so the window manager's press/drag/close paths are exercised, not just empty desktop.
WINS = [(300, 60, 336), (20, 140, 256), (40, 40, 272), (20, 210, 272)]
def hot():
    if rng.randrange(5) == 0:
        return rng.randrange(640), rng.randrange(228, 240)       # the taskbar
    x0, y0, w = rng.choice(WINS)
    if rng.randrange(2):
        return x0 + w - rng.randrange(1, 40), y0 + rng.randrange(0, 12)   # the boxes
    return x0 + rng.randrange(w), y0 + rng.randrange(0, 12)              # the title band
def point():
    if rng.randrange(2):
        px, py = hot()
        return min(32767, px * 32767 // 639), min(32767, py * 32767 // 239)
    return coord(), coord()
KINDS = [int(k) for k in os.environ.get('DFUZZ_KINDS', '0,1,2,3,4,5').split(',')]
def batch():
    evs = []
    kind = rng.choice(KINDS)
    if kind <= 1:  # a key, maybe chorded
        mods = [m for m in MODS if rng.randrange(4) == 0]
        k = rng.choice(KEYS)
        evs += [ev_key(m, True) for m in mods] + [ev_key(k, True), ev_key(k, False)] + [ev_key(m, False) for m in reversed(mods)]
    elif kind == 2:  # a pointer move
        x, y = point(); evs += [ev_abs('x', x), ev_abs('y', y)]
    elif kind == 3:  # a click, maybe double
        b = rng.choice(['left', 'left', 'right', 'middle'])
        x, y = point(); evs += [ev_abs('x', x), ev_abs('y', y)]
        for _ in range(rng.choice([1, 1, 2])):
            evs += [ev_btn(b, True), ev_btn(b, False)]
    elif kind == 4:  # a drag, PACED: a whole drag in one batch lands in one desktop tick, and the
        # window manager then sees press and release together and nothing moves (0 drags). A press
        # on a title band, then steps a tick apart, then the release.
        px, py = hot() if (os.environ.get('DFUZZ_DRAG_HOT', '1') == '1' and rng.randrange(3)) else (rng.randrange(640), rng.randrange(200))
        steps = [(px, py)]
        for _ in range(rng.randrange(2, 6)):
            px = max(0, min(639, px + rng.randrange(-80, 81))); py = max(0, min(239, py + rng.randrange(-40, 41)))
            steps.append((px, py))
        a = lambda v, m: min(32767, v * 32767 // m)
        return ('drag', [(a(x, 639), a(y, 239)) for x, y in steps])
    else:  # a wheel turn
        evs += [ev_btn(rng.choice(['wheel-up', 'wheel-down']), True), ev_btn('wheel-up', False), ev_btn('wheel-down', False)]
    return evs
# Deliberate first uses before any measurement: every managed window maximized and restored
# (Alt+F10 twice, focus moved with Alt+Tab), so the resize path pays its one-time buffers here
# rather than at a random moment inside the measured window.
def chord(mod, k):
    qcmd({'execute': 'input-send-event', 'arguments': {'events': [ev_key(mod, True), ev_key(k, True), ev_key(k, False), ev_key(mod, False)]}})
    time.sleep(.05)
for _ in range(6):
    chord('alt', 'f10'); chord('alt', 'f10'); chord('alt', 'tab')
sent = 0; n = 0; t0 = time.time()
# The first two thirds are WARM-UP, not measured: buffers that grow to a high-water mark once (a
# resize double-buffer, a history filling to its 32 entries, the spare surface pool, an event
# queue) are bounded builds, not leaks, and random input keeps finding new ones for a while
# (ADR-182, traced with the `heaptrace` feature). The claim is that the heap PLATEAUS: the last
# third of the storm must stay under DFUZZ_HEAP_BOUND.
warm = 2 * total // 3
measured = False
def send(evs):
    r = qcmd({'execute': 'input-send-event', 'arguments': {'events': evs}})
    if 'error' in r: raise SystemExit('FAIL: QMP refused an event: %s %s' % (r['error'], evs))
while sent < total:
    if not measured and sent >= warm:
        h0 = heap(); measured = True
    evs = batch()
    if isinstance(evs, tuple):  # a paced drag
        pts = evs[1]
        send([ev_abs('x', pts[0][0]), ev_abs('y', pts[0][1])]); time.sleep(.03)
        send([ev_btn('left', True)]); time.sleep(.03)
        for x, y in pts[1:]:
            send([ev_abs('x', x), ev_abs('y', y)]); time.sleep(.03)
        send([ev_btn('left', False)])
        sent += 2 * len(pts) + 2
    else:
        send(evs)
        sent += len(evs)
    if rng.randrange(25) == 0 or sent >= total:
        n += 1
        mark = len(txt())
        s.sendall(b'\recho DF-%d\r' % n)
        tag = '\nDF-%d\r' % n
        end = time.time() + 60
        while tag not in txt()[mark:]:
            if time.time() > end:
                raise SystemExit('FAIL: after %d events the console no longer answered the sentinel; last: %r' % (sent, txt()[-400:]))
            time.sleep(.05)
        for bad in BAD:
            if bad in txt()[start:]:
                raise SystemExit('FAIL: after %d events the machine printed %r; last: %r' % (sent, bad, txt()[-600:]))
h1 = heap()
grown = None if h0 is None or h1 is None else h1 - h0
mark = len(txt())
s.sendall(b'\rinput\r')
end = time.time() + 60
while 'windows:' not in txt()[mark:]:
    if time.time() > end: raise SystemExit('FAIL: after the storm `input` no longer answered')
    time.sleep(.1)
time.sleep(1)
readout = ' | '.join(l.strip() for l in txt()[mark:].splitlines() if l.strip() and 'aletheia>' not in l)
print('    %d random device events in %.0f s, %d sentinels answered, heap %s' % (sent, time.time() - t0, n, 'n/a' if grown is None else '+%d B' % grown))
if grown is not None and grown > DFUZZ_HEAP_BOUND:
    raise SystemExit('FAIL: the heap grew %d B across the storm (bound %d)' % (grown, DFUZZ_HEAP_BOUND))
print('    input: ' + readout[:900])
PY
  local rc=$?
  kill -9 "$pid" 2>/dev/null || true; trap - RETURN; rm -f "$qmp" "$ser"
  if [ "$rc" -eq 0 ]; then
    echo "DESKTOP-FUZZ-E2E-$arch: PASS"
  else
    echo "DESKTOP-FUZZ-E2E-$arch: FAIL (rc=$rc)"; echo "--- last 40 serial lines"; tail -40 "$log" 2>/dev/null
    fail=1
  fi
}

want aarch64 && { run_target aarch64 || fail=1; }
want riscv64 && { run_target riscv64 || fail=1; }
want x86_64 && { run_target x86_64 || fail=1; }
if [ "$fail" -eq 0 ]; then
  echo "DESKTOP-FUZZ-E2E: PASS — random keyboard and pointer storms never stopped the machine answering, nothing panicked"
  exit 0
fi
echo "DESKTOP-FUZZ-E2E: FAIL"
exit 1
