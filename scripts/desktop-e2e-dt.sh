#!/usr/bin/env bash
# Live GUI workflow gate for the aarch64 and RISC-V DT targets (ADR-127).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
[ -x "$HOME/.cargo/bin/cargo" ] && export PATH="$HOME/.cargo/bin:$PATH"

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
  command -v "$qemu" >/dev/null 2>&1 || { echo "DESKTOP-E2E-$arch: SKIP (missing $qemu)"; return 0; }

  echo "==> [$arch] build interactive kernel"
  (cd "$dir" && cargo build --features interactive)
  img="$dir/target/desktop-e2e.img"
  dd if=/dev/zero of="$img" bs=1048576 count=1 2>/dev/null
  local pciimg="$dir/target/desktop-e2e-pci.img"
  dd if=/dev/zero of="$pciimg" bs=1048576 count=1 2>/dev/null
  qmp="${TMPDIR:-/tmp}/aletheia-desktop-$arch-$$.qmp"; ser="${TMPDIR:-/tmp}/aletheia-desktop-$arch-$$.ser"
  log="$dir/target/desktop-e2e-$arch.log"; rm -f "$qmp" "$ser" "$log"
  rootbin="$dir/target/capvault-root.bin"
  printf 'aletheia-capvault-root-0123456789abcdef' | head -c 32 > "$rootbin"

  local -a args=("$qemu" -machine "$machine" -cpu "$cpu" -smp 4 -m 128M -display none -S
    -global virtio-mmio.force-legacy=false
    -drive "if=none,format=raw,file=$img,id=blk0" -device virtio-blk-device,drive=blk0
    -drive "if=none,format=raw,file=$pciimg,id=pciblk0" -device virtio-blk-pci,disable-legacy=on,drive=pciblk0
    -device virtio-gpu-device -device virtio-keyboard-device -device virtio-tablet-device
    -kernel "$elf" -chardev "socket,id=ser0,path=$ser,server=on,wait=off" -serial chardev:ser0
    -qmp "unix:$qmp,server,nowait")
  if [ "$arch" = aarch64 ]; then
    dtbraw="$dir/target/desktop-e2e-dtb-raw.bin"; dtb="$dir/target/desktop-e2e-dtb.bin"
    "$qemu" -machine "$machine" -global arm-smmuv3.stage=2 -cpu "$cpu" -smp 4 -m 128M \
      -device virtio-gpu-device -device virtio-keyboard-device -device virtio-tablet-device \
      -machine dumpdtb="$dtbraw" >/dev/null 2>&1
    set -- $(od -An -tu1 -j4 -N4 "$dtbraw")
    local tsz=$(( $1 << 24 | $2 << 16 | $3 << 8 | $4 )); head -c "$tsz" "$dtbraw" > "$dtb"
    args+=( -global arm-smmuv3.stage=2 -fw_cfg "name=opt/org.aletheia/capvault-root,file=$rootbin" -fw_cfg "name=opt/org.aletheia/dtb,file=$dtb" )
  else
    args+=( -bios default )
  fi

  "${args[@]}" & local pid=$!
  trap 'kill -9 "$pid" 2>/dev/null || true; rm -f "$qmp" "$ser"' RETURN

  python3 - "$qmp" "$ser" "$log" "$dir/target/desktop-e2e-$arch.ppm" "$ROOT/kernel-core/assets/wallpaper-640x240.bgr" <<'PY'
import json, re, socket, sys, threading, time
qmp, serial, log, shot, photo = sys.argv[1:]
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
    if 'error' in r: raise RuntimeError(r['error'])
    time.sleep(.15)
def key(k):
    send([{'type':'key','data':{'down':True,'key':{'type':'qcode','data':k}}},{'type':'key','data':{'down':False,'key':{'type':'qcode','data':k}}}])
def keys(s):
    for c in s:
        key(c.lower() if c.isalpha() else ('spc' if c==' ' else 'ret'))
def command(c):
    n=len(txt()); s.sendall(c.encode()+b'\r'); end=time.time()+30
    while time.time()<end:
        tail=txt()[n:]
        # The startup prompt can race this first command: wait for command-owned output as well,
        # rather than mistaking the prompt printed before the UART interrupt path was armed for
        # the response to this command.
        if 'session:' in tail and 'aletheia>' in tail: return tail
        time.sleep(.1)
    raise RuntimeError('serial command timed out: '+c)
# The machine is ASYNCHRONOUS: a QMP-injected event reaches the device, and the desktop's pump
# picks it up on its own tick. Asking once and asserting is a race that a loaded runner loses, so
# every assertion about a device event re-reads the machine's own readout until it agrees or a
# bound is spent. The bound is what turns "slow" into a failure instead of a hang.
def eventually(pred, why, secs=20):
    end=time.time()+secs; last=''
    while time.time()<end:
        last=command('input')
        if pred(last): return last
    raise RuntimeError('the machine never reported '+why+': '+last[-300:])
t=txt(); assert '[desktop] LIVE:' in t; assert 'ALL 10 INPUT-HARDWARE INVARIANTS HOLD' in t
# The photograph behind the wallpaper (ADR-175) must reach the DISPLAY, not just the model: dump
# the scanout through QMP and count the pixels that equal the asset's pixel at the same place.
# Windows cover part of it and the panel's ink border covers the edge, so the bound is a share,
# and a 1-bit desktop (no photo) matches almost nothing.
def photo_share():
    r=qcmd({'execute':'screendump','arguments':{'filename':shot}})
    if 'error' in r: raise RuntimeError(r['error'])
    raw=open(shot,'rb').read(); parts=raw.split(b'\n',3)
    w,h=map(int,parts[1].split()); px=parts[3]; bgr=open(photo,'rb').read()
    assert (w,h)==(640,240), (w,h)
    hit=sum(1 for i in range(w*h) if px[3*i]==bgr[3*i+2] and px[3*i+1]==bgr[3*i+1] and px[3*i+2]==bgr[3*i])
    return hit/(w*h)
share=0.0
for _ in range(50):
    share=photo_share()
    if share>=0.25: break
    time.sleep(.2)
print(f'wallpaper photo share of the scanout: {share:.1%} ({shot})')
assert share>=0.25, f'the photograph did not reach the display: {share:.1%} of pixels match'
o=command('input'); m=re.search(r'events posted (\d+) dropped (\d+)',o); assert m and m.group(1)=='0' and m.group(2)=='0'
send([{'type':'abs','data':{'axis':'x','value':16384}},{'type':'abs','data':{'axis':'y','value':16384}}])
eventually(lambda o: 'cursor: (320, 120) shown' in o, 'the pointer move')
send([{'type':'btn','data':{'button':'left','down':True}}]); send([{'type':'btn','data':{'button':'left','down':False}}])
eventually(lambda o: re.search(r'focus: surface \d+ \(0 queued\)',o) is not None, 'the click')
keys('help\n'); assert wait('commands:',30)
# Exercise a desktop-only action through the real keyboard path; Alt+F9 must be consumed by the desktop.
send([{'type':'key','data':{'down':True,'key':{'type':'qcode','data':'alt'}}},{'type':'key','data':{'down':True,'key':{'type':'qcode','data':'f9'}}},{'type':'key','data':{'down':False,'key':{'type':'qcode','data':'f9'}}},{'type':'key','data':{'down':False,'key':{'type':'qcode','data':'alt'}}}])
eventually(lambda o: 'windows:' in o, 'the window set after Alt+F9')
print('DESKTOP LIVE E2E: PASS')
PY
  kill -9 "$pid" 2>/dev/null || true; trap - RETURN; rm -f "$qmp" "$ser"
  echo "DESKTOP-E2E-$arch: PASS"
}

run_target aarch64
run_target riscv64
echo "DESKTOP-E2E-DT: PASS"
