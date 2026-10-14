#!/usr/bin/env bash
# A USB keyboard as console input, LIVE (REQ-DRV-014, ADR-255).
#
# The boot gates prove the xHCI driver enumerates and configures a USB boot keyboard (ADR-253);
# this rung proves the keyboard TYPES: an interactive x86-64 machine keeps the keyboard, and keys
# QEMU delivers to that USB keyboard - and only to it - reach the shell and run.
#
# Routing is explicit, not luck: the usb-kbd is bound to the display `vga0`, and every key is sent
# with QMP `input-send-event` naming `vga0`, so QEMU delivers it to the bound USB keyboard and to no
# other input handler. The serial line is a FILE with no writer. A keystroke that reaches the
# console can only have come through the xHCI controller, its interrupt-IN endpoint, the boot
# report decoder and the console's USB poll.
#
# Exit 0 PASS. SKIP (exit 0, never silent pass) when the host lacks QEMU/OVMF/python.
set -uo pipefail

if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
X86="$ROOT/kernel-x86_64"
BUILD="$X86/build"
LOG="$BUILD/usbkbd-e2e.log"
QMP="${TMPDIR:-/tmp}/aletheia-usbkbd-e2e-$$.qmp"

hr() { printf '========================================================================\n'; }

OVMF_CODE_PATH=""
for c in "${OVMF_CODE:-}" /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd \
         /opt/homebrew/share/qemu/edk2-x86_64-code.fd /usr/share/edk2/x64/OVMF_CODE.4m.fd; do
  [ -n "$c" ] && [ -f "$c" ] && { OVMF_CODE_PATH="$c"; break; }
done
OVMF_VARS_PATH=""
for v in "${OVMF_VARS:-}" /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd \
         /opt/homebrew/share/qemu/edk2-i386-vars.fd /usr/share/edk2/x64/OVMF_VARS.4m.fd; do
  [ -n "$v" ] && [ -f "$v" ] && { OVMF_VARS_PATH="$v"; break; }
done

PY="$(command -v python3 || command -v python)"
if ! command -v qemu-system-x86_64 >/dev/null 2>&1 || [ -z "$OVMF_CODE_PATH" ] \
   || [ -z "$OVMF_VARS_PATH" ] || [ -z "$PY" ]; then
  echo "SKIP: needs qemu-system-x86_64 + OVMF + python3 (this rung did NOT run)"
  echo "USBKBD-E2E: SKIP"
  exit 0
fi

hr; echo "==> building the UEFI image WITH the interactive console"; hr
mkdir -p "$BUILD"
( cd "$X86" && cargo build --release --features interactive ) \
  || { echo "FAIL: build"; echo "USBKBD-E2E: FAIL"; exit 1; }
IMG="$BUILD/aletheia-usbkbd.img"
"$PY" "$X86/scripts/mkesp.py" \
  --efi "$X86/target/x86_64-unknown-uefi/release/aletheia-kernel-x86_64.efi" \
  --out "$IMG" >/dev/null \
  || { echo "FAIL: image build"; echo "USBKBD-E2E: FAIL"; exit 1; }

rm -f "$LOG" "$QMP"
VARS="$BUILD/usbkbd-e2e-vars.fd"
cp "$OVMF_VARS_PATH" "$VARS"

hr; echo "==> booting; the operator types on a USB keyboard behind qemu-xhci"; hr
qemu-system-x86_64 -machine q35 -m 256 -cpu qemu64,+smep -display none \
  -drive if=pflash,format=raw,unit=0,readonly=on,file="$OVMF_CODE_PATH" \
  -drive if=pflash,format=raw,unit=1,file="$VARS" \
  -drive format=raw,file="$IMG" \
  -vga none -device VGA,id=vga0 \
  -device qemu-xhci,id=xhci -device usb-kbd,bus=xhci.0,id=usbkbd0,display=vga0 \
  -serial "file:$LOG" \
  -qmp "unix:$QMP,server,nowait" &
QEMU_PID=$!
trap 'kill -9 "$QEMU_PID" 2>/dev/null; rm -f "$QMP"' EXIT

"$PY" - "$QMP" "$LOG" <<'PYEOF'
import json, socket, sys, time

qmp_path, log_path = sys.argv[1], sys.argv[2]

def log_text():
    try:
        with open(log_path, 'rb') as f:
            return f.read().decode('utf-8', 'replace')
    except FileNotFoundError:
        return ''

def wait_for(needle, timeout=180):
    end = time.time() + timeout
    while time.time() < end:
        if needle in log_text():
            return True
        time.sleep(0.25)
    return False

end = time.time() + 60
sock = None
while time.time() < end and sock is None:
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(qmp_path)
        sock = s
    except OSError:
        time.sleep(0.25)
if sock is None:
    print("FAIL: QMP socket never appeared"); sys.exit(1)
f = sock.makefile('rwb')

def cmd(obj):
    f.write((json.dumps(obj) + '\n').encode()); f.flush()
    while True:
        line = f.readline()
        if not line:
            raise RuntimeError('QMP closed')
        msg = json.loads(line)
        if 'return' in msg or 'error' in msg:
            return msg

f.readline()
cmd({'execute': 'qmp_capabilities'})

def key(name, down):
    r = cmd({'execute': 'input-send-event', 'arguments': {
        'device': 'vga0',
        'events': [{'type': 'key', 'data': {'down': down, 'key': {'type': 'qcode', 'data': name}}}],
    }})
    if 'error' in r:
        raise RuntimeError('input-send-event refused: %r' % r['error'])
    time.sleep(0.04)

NAMES = {' ': 'spc', '-': 'minus', '.': 'dot', '/': 'slash'}
def press(*names):
    for n in names:
        key(n, True)
    for n in reversed(names):
        key(n, False)

def typed(text):
    for ch in text:
        if ch.isupper():
            press('shift', ch.lower())
        else:
            press(NAMES.get(ch, ch))

def enter():
    press('ret')
    time.sleep(0.6)

fails = []
def check(ok, name):
    print(('  [pass] ' if ok else '  [FAIL] ') + name)
    if not ok:
        fails.append(name)

if not wait_for('Aletheia interactive console', timeout=240):
    print('FAIL: the console never started'); print(log_text()[-3000:]); sys.exit(1)
wait_for('aletheia>', timeout=60)
time.sleep(0.5)

text = log_text()
check('ALL 7 XHCI INVARIANTS HOLD' in text, 'xhci: the boot suite proved the controller and keyboard')
check('USB keyboard kept as console input' in text, 'xhci: the interactive machine kept the keyboard')

mark = len(log_text())
typed('echo Usb')
enter()
after = log_text()[mark:]
check('echo Usb' in after, 'usb: keys reach the line editor (shift held gives an uppercase byte)')
check('\nUsb' in after.replace('\r', ''), 'usb: Enter runs the line')

mark = len(log_text())
typed('write usbtest hello')
enter()
typed('cat usbtst')
press('left')
press('left')
typed('e')
press('end')
enter()
after = log_text()[mark:]
check('hello' in after and '[D' not in after,
      'usb: an arrow key edits the line, and a later command reads back what an earlier one wrote')

typed('halt')
enter()

print()
if fails:
    print('FAILED: ' + '; '.join(fails))
    print(log_text()[-4000:])
    sys.exit(1)
print('every keystroke travelled usb-kbd -> xHCI interrupt-IN -> boot report -> decoder -> line editor')
sys.exit(0)
PYEOF
rc=$?

kill -9 "$QEMU_PID" 2>/dev/null
wait "$QEMU_PID" 2>/dev/null

hr
if [ "$rc" -eq 0 ]; then
  echo "USBKBD-E2E: PASS"
else
  echo "USBKBD-E2E: FAIL"
fi
exit "$rc"
