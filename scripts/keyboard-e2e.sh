#!/usr/bin/env bash
# A person at the machine's own keyboard (REQ-CON-003, ADR-049).
#
# `console-e2e.sh` proves the console by typing at the SERIAL LINE. That is the input source
# Aletheia already had, and it is precisely why ALET-P2-039 went unnoticed: under `-serial stdio` the
# terminal IS the wire, so a kernel with no keyboard driver looks identical to one with a working
# keyboard. A gate that types on the wire can never find that bug.
#
# This one types on the KEYBOARD. QEMU's `sendkey` injects at the emulated i8042, so every keystroke
# travels the real path — controller output buffer, IRQ1, the PIC, vector 0x21, the scancode decoder,
# the shared input ring, the line editor. Nothing about the test knows what a scancode is; it presses
# keys by NAME and asserts that Aletheia's own filesystem changed.
#
# What is asserted, and why each one is a different failure:
#   1. the boot's keyboard suite found the controller through ACPI and passed  (driver)
#   2. characters typed at the keyboard appear at the prompt                   (IRQ + decode + echo)
#   3. shift produces an uppercase byte                                        (held modifier state)
#   4. backspace removes exactly one character                                 (editor sees 0x08)
#   5. Enter EXECUTES the line, and the command's effect is real               (editor sees CR)
#   6. the effect is visible to a command typed afterwards                     (the OS actually did it)
#
# Exit 0 = PASS. SKIP (exit 0, never a silent pass) when the host lacks QEMU/OVMF/mtools.
set -uo pipefail

# Honor the per-crate nightly toolchain via the rustup shim (a Homebrew/system cargo earlier in
# PATH ignores rust-toolchain.toml and fails cross-compilation with E0463).
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
X86="$ROOT/kernel-x86_64"
BUILD="$X86/build"
LOG="$BUILD/keyboard-e2e.log"
# NOT under $BUILD: a repository checked out on a Windows filesystem is reached through drvfs, which
# cannot host a Unix domain socket at all ("Operation not supported" at bind). The control channel
# goes to the host's own temp directory; the serial log stays beside the build, where a human looks.
QMP="${TMPDIR:-/tmp}/aletheia-keyboard-e2e-$$.qmp"

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
  echo "KEYBOARD-E2E: SKIP"
  exit 0
fi

hr; echo "==> building the UEFI image WITH the interactive console"; hr
( cd "$X86" && cargo build --release --features interactive ) \
  || { echo "FAIL: build"; echo "KEYBOARD-E2E: FAIL"; exit 1; }
IMG="$BUILD/aletheia-keyboard.img"
"$PY" "$X86/scripts/mkesp.py" \
  --efi "$X86/target/x86_64-unknown-uefi/release/aletheia-kernel-x86_64.efi" \
  --out "$IMG" >/dev/null \
  || { echo "FAIL: image build"; echo "KEYBOARD-E2E: FAIL"; exit 1; }

rm -f "$LOG" "$QMP"
VARS="$BUILD/keyboard-e2e-vars.fd"
cp "$OVMF_VARS_PATH" "$VARS"

hr; echo "==> booting; the operator will type on the emulated i8042, not on the wire"; hr
# `-display none` and NO `-serial stdio`: the serial line is a FILE, so nothing can be typed at it.
# If a keystroke reaches the console, it can only have come through the keyboard controller.
qemu-system-x86_64 -machine q35 -m 256 -cpu qemu64,+smep -display none \
  -drive if=pflash,format=raw,unit=0,readonly=on,file="$OVMF_CODE_PATH" \
  -drive if=pflash,format=raw,unit=1,file="$VARS" \
  -drive format=raw,file="$IMG" \
  -serial "file:$LOG" \
  -qmp "unix:$QMP,server,nowait" &
QEMU_PID=$!
trap 'kill -9 "$QEMU_PID" 2>/dev/null' EXIT

"$PY" - "$QMP" "$LOG" <<'PYEOF'
import json, os, socket, sys, time

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

# The socket appears as soon as QEMU is up; the guest takes longer.
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

f.readline()                      # the greeting
cmd({'execute': 'qmp_capabilities'})

# QMP key names. `shift` is sent as a MODIFIER inside one sendkey, which is what a real keyboard
# does — press shift, press the key, release both — and is therefore what exercises the decoder's
# held-modifier state rather than a synthetic uppercase byte.
NAMES = {
    ' ': 'spc', '-': 'minus', '.': 'dot', '/': 'slash', ',': 'comma',
    "'": 'apostrophe', ';': 'semicolon', '=': 'equal',
}
def keys_for(ch):
    if ch.isupper():
        return ['shift', NAMES.get(ch.lower(), ch.lower())]
    if ch.isdigit() or ch.islower():
        return [ch]
    if ch in NAMES:
        return [NAMES[ch]]
    raise ValueError('no QMP key name for %r' % ch)

def press(keys, hold_ms=30):
    cmd({'execute': 'send-key', 'arguments': {
        'keys': [{'type': 'qcode', 'data': k} for k in keys],
        'hold-time': hold_ms,
    }})
    time.sleep(0.06)

def typed(text):
    for ch in text:
        press(keys_for(ch))

def enter():
    press(['ret'])
    time.sleep(0.6)

fails = []
def check(ok, name):
    print(('  [pass] ' if ok else '  [FAIL] ') + name)
    if not ok:
        fails.append(name)

if not wait_for('Aletheia interactive console', timeout=240):
    print('FAIL: the console never started'); print(log_text()[-3000:]); sys.exit(1)
# The prompt is the signal that the input ring is being read.
wait_for('aletheia>', timeout=60)
time.sleep(0.5)

# 1 — the driver found the controller through the firmware's own declaration.
text = log_text()
check('KEYBOARD INVARIANTS HOLD' in text, 'ps2: the boot brought the controller up and proved it')
check('IAPC_BOOT_ARCH' in text, 'ps2: the controller was found through ACPI, not by poking a port')

# 2 + 3 — type a mixed-case word. Nothing can reach the console except through the keyboard: the
# serial line is a file with no writer.
mark = len(log_text())
typed('echo Kb')
time.sleep(0.4)
after = log_text()[mark:]
check('echo Kb' in after, 'keyboard: characters typed at the i8042 reach the line editor')
check('K' in after, 'keyboard: shift produces an uppercase byte (held modifier state)')

# 4 — backspace removes exactly one character, then retype it.
press(['backspace'])
time.sleep(0.3)
typed('b')
enter()
out = log_text()[mark:]
check('Kb' in out, 'keyboard: backspace removes exactly one character')

# 5 + 6 — Enter EXECUTES, and the effect is real: write an object, then read it back with a second
# typed command. A console that echoed keys but never ran them would pass 2-4 and fail here.
mark = len(log_text())
typed('write kbtest hello')
enter()
after = log_text()[mark:]
check('wrote' in after, 'keyboard: Enter executes the line (the editor saw a carriage return)')

mark = len(log_text())
typed('cat kbtest')
enter()
after = log_text()[mark:]
check('hello' in after, 'keyboard: the effect is real — a later typed command reads it back')

# 7 — THE regression (REQ-CON-004, ADR-050). The arrow keys are `E0`-prefixed scancodes, and the
# console used to drop the `ESC` its decoder never sent and type the rest as text. Pressing left four
# times and then a character must EDIT the line, not append `[D[D[D[D` to it. Nothing about this
# test knows what an escape sequence is: it presses `left` by name and reads Aletheia's own output.
mark = len(log_text())
typed('cat kbtst')          # deliberately missing the 'e', in the MIDDLE of the name
press(['left'])             # ...so repairing it needs the cursor to move, twice
press(['left'])
typed('e')
press(['end'])
enter()
after = log_text()[mark:]
check('hello' in after and '[D' not in after,
      'keyboard: an arrow key edits the line instead of typing escape characters into it')

# 8 — history. The up arrow recalls the previous line, and Enter runs it again.
mark = len(log_text())
press(['up'])
time.sleep(0.3)
enter()
after = log_text()[mark:]
check('hello' in after and '[A' not in after,
      'keyboard: the up arrow recalls the previous command and runs it')

# 9 — Tab completes against the real namespace. `cat kbt<TAB>` can only mean `kbtest`.
mark = len(log_text())
typed('cat kbt')
press(['tab'])
time.sleep(0.3)
enter()
after = log_text()[mark:]
check('hello' in after, 'keyboard: Tab completes an object name from the live namespace')

# 10 — Home/End and Delete reach the ends of the line and remove under the cursor. A junk character
# typed at the front is removed with Home + Delete, and the surviving command still runs.
mark = len(log_text())
typed('xcat kbtest')
press(['home'])
press(['delete'])
enter()
after = log_text()[mark:]
check('hello' in after and 'unknown command' not in after,
      'keyboard: Home and Delete repair a line typed wrongly at its start')

typed('halt')
enter()

print()
if fails:
    print('FAILED: ' + '; '.join(fails))
    print(log_text()[-4000:])
    sys.exit(1)
print('every keystroke travelled controller -> IRQ1 -> decoder -> ring -> line editor')
sys.exit(0)
PYEOF
rc=$?

kill -9 "$QEMU_PID" 2>/dev/null
wait "$QEMU_PID" 2>/dev/null

hr
if [ "$rc" -eq 0 ]; then
  echo "KEYBOARD-E2E: PASS"
else
  echo "KEYBOARD-E2E: FAIL"
fi
exit "$rc"
