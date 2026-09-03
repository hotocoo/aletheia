#!/usr/bin/env bash
# The input HARDWARE rung, LIVE (ALET-P2-021's device rung, ADR-080).
#
# `keyboard-e2e.sh` proves the i8042 wire: keystrokes reach the line editor through the
# controller. It could not prove THIS rung, because this rung is not about the console —
# it is about the DESKTOP: a virtio-input keyboard and a virtio-input tablet wired through
# the compositor's input session, with the machine's cursor steered by real pointing-device
# events and clicks routing focus. The boot gates prove the driver against the real devices
# with synthetic records; this gate drives the LIVE pump with events QEMU itself injects:
#
#   1. the boot's input-hardware suite passed against the real devices        (driver)
#   2. the live desktop came up and reports its session through `input`,
#      with NOTHING posted yet — the hardware wire starts silent               (session)
#   3. an absolute pointer event moves the machine's cursor to the MAPPED
#      position the device's own axis range implies                           (pointer)
#   4. a click focuses the surface under the point; a click on empty space
#      clears focus and the loser is TOLD through its own queue; clicks post
#      no keystroke                                                           (routing)
#   5. a keystroke on the virtio keyboard reaches the SESSION (posted, exactly
#      one byte), the window's owner - the console - drains it and ECHOES it:
#      the terminal window is the console's second surface (ADR-083); a whole
#      `help` typed on the virtio keyboard is answered by the same shell        (keyboard)
#   5b. the `input` readout shows the window's placement and the terminal's
#      last line; a drag by the title band moves the window by the pointer's
#      delta                                                                  (window)
#   6. with nothing happening, the counters hold still                        (quiet)
#
# Two wires, on purpose. QEMU routes injected key events to its ACTIVE keyboard handler,
# and the virtio keyboard becomes that handler the moment the driver sets DRIVER_OK — so the
# i8042 cannot be typed on from here once the desktop is up (a first version of this gate
# tried, and every `input` it typed landed in the window's queue instead of on the console).
# The console is therefore driven over the SERIAL wire: COM1 is a socket the harness connects
# to BEFORE the guest runs (`-S`, then `cont`), so every byte the machine prints — boot log
# and readouts alike — lands in the log file with no other writer, and the assertions read
# the machine's own `input` ledger.
#
# Exit 0 = PASS. SKIP (exit 0, never a silent pass) when the host lacks QEMU/OVMF/python.
set -uo pipefail

# Honor the per-crate nightly toolchain via the rustup shim (a Homebrew/system cargo earlier in
# PATH ignores rust-toolchain.toml and fails cross-compilation with E0463).
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
X86="$ROOT/kernel-x86_64"
BUILD="$X86/build"
LOG="$BUILD/vinput-e2e.log"
QMP="${TMPDIR:-/tmp}/aletheia-vinput-e2e-$$.qmp"
SER="${TMPDIR:-/tmp}/aletheia-vinput-e2e-$$.ser"

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
  echo "VINPUT-E2E: SKIP"
  exit 0
fi

hr; echo "==> building the UEFI image WITH the interactive console"; hr
( cd "$X86" && cargo build --release --features interactive ) \
  || { echo "FAIL: build"; echo "VINPUT-E2E: FAIL"; exit 1; }
IMG="$BUILD/aletheia-vinput.img"
"$PY" "$X86/scripts/mkesp.py" \
  --efi "$X86/target/x86_64-unknown-uefi/release/aletheia-kernel-x86_64.efi" \
  --out "$IMG" >/dev/null \
  || { echo "FAIL: image build"; echo "VINPUT-E2E: FAIL"; exit 1; }

rm -f "$LOG" "$QMP" "$SER"
VARS="$BUILD/vinput-e2e-vars.fd"
cp "$OVMF_VARS_PATH" "$VARS"

hr; echo "==> booting (paused) with virtio-input keyboard + tablet + GPU; COM1 is a socket the harness owns"; hr
# `-S`: the CPU starts paused, so the harness can attach to the serial socket and the QMP
# socket before the first byte is printed; nothing of the boot log is lost to an unconnected
# chardev. The harness alone writes the log file, from bytes the machine alone produced.
qemu-system-x86_64 -machine q35 -m 256 -cpu qemu64,+smep -display none -S \
  -drive if=pflash,format=raw,unit=0,readonly=on,file="$OVMF_CODE_PATH" \
  -drive if=pflash,format=raw,unit=1,file="$VARS" \
  -drive format=raw,file="$IMG" \
  -device virtio-gpu-pci,disable-legacy=on \
  -device virtio-keyboard-pci \
  -device virtio-tablet-pci \
  -chardev "socket,id=ser0,path=$SER,server=on,wait=off" \
  -serial chardev:ser0 \
  -qmp "unix:$QMP,server,nowait" &
QEMU_PID=$!
trap 'kill -9 "$QEMU_PID" 2>/dev/null; rm -f "$QMP" "$SER"' EXIT

"$PY" - "$QMP" "$SER" "$LOG" <<'PYEOF'
import json, re, socket, sys, threading, time

qmp_path, ser_path, log_path = sys.argv[1], sys.argv[2], sys.argv[3]

# The PINNED axis range the boot suite asserts the tablet declares (vinput.rs, ADR-080) and
# the framebuffer console's scanout the desktop composes over. The expected cursor position
# is computed from THESE, not hardcoded per point: same formula as the kernel's mapping.
AXIS_MAX = 32767
SCAN_W, SCAN_H = 640, 240

def map_axis(v, span):
    return min((v * span) // (AXIS_MAX + 1), span - 1)

def connect_unix(path, what, timeout=60):
    end = time.time() + timeout
    while time.time() < end:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            s.connect(path)
            return s
        except OSError:
            s.close()
            time.sleep(0.2)
    print("FAIL: %s socket never appeared" % what); sys.exit(1)

# --- QMP -----------------------------------------------------------------------------------
sock = connect_unix(qmp_path, 'QMP')
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

# --- the serial wire: every byte the machine prints goes to the log; the console is typed here
ser = connect_unix(ser_path, 'serial')
logf = open(log_path, 'ab')
def reader():
    while True:
        try:
            data = ser.recv(4096)
        except OSError:
            break
        if not data:
            break
        logf.write(data); logf.flush()
threading.Thread(target=reader, daemon=True).start()

def log_text():
    try:
        with open(log_path, 'rb') as fh:
            return fh.read().decode('utf-8', 'replace')
    except FileNotFoundError:
        return ''

def wait_for(needle, timeout=240):
    end = time.time() + timeout
    while time.time() < end:
        if needle in log_text():
            return True
        time.sleep(0.25)
    return False

# Both sockets are attached: let the machine run.
cmd({'execute': 'cont'})

fails = []
def check(ok, name):
    print(('  [pass] ' if ok else '  [FAIL] ') + name)
    if not ok:
        fails.append(name)

# --- the hardware wire: QMP-injected events reach QEMU's ACTIVE handlers — the virtio
#     keyboard (once DRIVER_OK) and the virtio tablet (the only absolute pointer).
def press(keys, hold_ms=30):
    events = []
    for k in keys:
        events.append({'type': 'key', 'data': {'down': True, 'key': {'type': 'qcode', 'data': k}}})
    for k in reversed(keys):
        events.append({'type': 'key', 'data': {'down': False, 'key': {'type': 'qcode', 'data': k}}})
    cmd({'execute': 'input-send-event', 'arguments': {'events': events}})
    time.sleep(0.06)

def pointer(events, hold_ms=80):
    # ONE input-send-event = ONE sync = one batch at the device, exactly what the decoder
    # commits on EV_SYN.
    cmd({'execute': 'input-send-event', 'arguments': {'events': events}})
    time.sleep(hold_ms / 1000.0)

def abs_move(x, y):
    return pointer([
        {'type': 'abs', 'data': {'axis': 'x', 'value': x}},
        {'type': 'abs', 'data': {'axis': 'y', 'value': y}},
    ])

def click():
    pointer([{'type': 'btn', 'data': {'button': 'left', 'down': True}}])
    pointer([{'type': 'btn', 'data': {'button': 'left', 'down': False}}])

# --- the console wire: `input`, typed on COM1, answered on COM1 ---------------------------
FACTS_RE = re.compile(r'events posted (\d+) dropped (\d+) refused (\d+)')
CURSOR_RE = re.compile(r'cursor: \((\d+), (\d+)\) (shown|hidden)')
FOCUS_RE = re.compile(r'focus: surface (\d+) \((\d+) queued\)|focus: none')
WIN_RE = re.compile(r'window: at \((-?\d+), (-?\d+)\)')
TERM_RE = re.compile(r'terminal: (\d+) lines, last "([^"]*)"')
WINDOWS_RE = re.compile(r'windows: (\d+) open, (\d+) closed, (\d+) drags')

def run_input():
    """Type `input` + Enter on the serial wire and return the parsed readout."""
    mark = len(log_text())
    ser.sendall(b'input\r')
    end = time.time() + 30
    while time.time() < end:
        chunk = log_text()[mark:]
        if FACTS_RE.search(chunk) and CURSOR_RE.search(chunk) and FOCUS_RE.search(chunk):
            break
        time.sleep(0.2)
    time.sleep(0.2)                # let the prompt land too
    chunk = log_text()[mark:]
    facts = FACTS_RE.search(chunk)
    cursor = CURSOR_RE.search(chunk)
    focus = FOCUS_RE.search(chunk)
    if focus and focus.group(1) is not None:
        focus_v, queued = 'surface ' + focus.group(1), int(focus.group(2))
    elif focus:
        focus_v, queued = 'none', 0
    else:
        focus_v, queued = None, None
    return chunk, (
        int(facts.group(1)) if facts else None,
        int(facts.group(2)) if facts else None,
        int(facts.group(3)) if facts else None,
        (int(cursor.group(1)), int(cursor.group(2)), cursor.group(3)) if cursor else None,
        focus_v,
        queued,
    )

if not wait_for('Aletheia interactive console', timeout=300):
    print('FAIL: the console never started'); print(log_text()[-3000:]); sys.exit(1)
wait_for('aletheia>', timeout=60)
time.sleep(0.5)

# 1 — the driver proved itself against the REAL devices during boot.
text = log_text()
check('ALL 10 INPUT-HARDWARE INVARIANTS HOLD' in text,
      'vinput: the boot suite proved the real devices (identity pinned, DMA-gated, silent)')
check('devices: ' in text and 'QEMU Virtio Keyboard' in text,
      'vinput: the devices answered for their identity by name')
check('desktop] LIVE' in text,
      'desktop: the live desktop came up over the real scanout')

# 2 — the session reports through the console; nothing has been typed on the hardware wire
#     yet, so the ledger is at zero; the cursor sits where install put it, the window is focused.
chunk, (posted0, dropped0, refused0, cursor0, focus0, queued0) = run_input()
check(posted0 is not None, 'input: the session reports its ledger to a human')
check(posted0 == 0 and dropped0 == 0 and queued0 == 0,
      'input: the hardware wire starts silent (posted %r dropped %r queued %r)' % (posted0, dropped0, queued0))
check(cursor0 == (320, 120, 'shown'),
      'input: the cursor starts where the desktop put it (%r)' % (cursor0,))
check(focus0 == 'surface 2', 'input: the window holds focus at start (%r)' % (focus0,))

# 3 — a REAL pointer event moves the machine's cursor to the mapped position. The sample is
# chosen to land at the CENTER of the window: (20480, 13653) maps to (400, 100) on this
# scanout, through the tablet's own declared axis range.
abs_move(20480, 13653)
chunk, (posted1, _d1, _r1, cursor1, focus1, _q1) = run_input()
check(cursor1 == (map_axis(20480, SCAN_W), map_axis(13653, SCAN_H), 'shown'),
      'pointer: the cursor moved to the mapped position (%r)' % (cursor1,))

# 4 — a click routes focus: over the window it keeps it; over the GAP between the surfaces
#     it clears; back over the window it takes it again. Clicks are routing, never keystrokes.
click()
chunk, (_p2, _d2, _r2, _c2, focus2, _q2) = run_input()
check(focus2 == 'surface 2', 'pointer: a click over the window holds focus on it (%r)' % (focus2,))
abs_move(30720, 27170)    # the gap at (600, 199)
click()
chunk, (_p3, _d3, _r3, _c3, focus3, _q3) = run_input()
check(focus3 == 'none', 'pointer: a click on empty space clears focus (%r)' % (focus3,))
abs_move(20480, 13653)
click()
chunk, (posted4, _d4, _r4, _c4, focus4, queued4) = run_input()
check(focus4 == 'surface 2', 'pointer: a click takes focus back to the window (%r)' % (focus4,))
# Routing, not typing: no byte was posted by any click — but the window that LOST focus to the
# empty-space click was TOLD, through its own queue, so exactly one FocusLost waits there.
check(posted4 == 0 and queued4 == 0,
      'pointer: clicks post no keystroke, and the window read what it was told (posted %r queued %r)' % (posted4, queued4))

# 5 — a keystroke on the virtio keyboard reaches the SESSION and not the console: one press of
#     `a` posts exactly ONE byte into the focused window's queue (where it stays — the window is
#     the only principal that may drain it), and the console, whose wire this is not, prints
#     nothing in between.
mark = len(log_text())
press(['a'])
time.sleep(0.5)
between = log_text()[mark:]
# The `a` now sits on the console's line (the window IS the console); repair it with a virtio
# backspace before asking `input`, so the readout is a command and not `ainput`. Two bytes
# were routed: the letter and the backspace.
press(['backspace'])
time.sleep(0.3)
chunk, (posted5, _d5, _r5, _c5, focus5, queued5) = run_input()
check(posted5 == posted4 + 2,
      'keyboard: the virtio keystroke and its backspace each posted exactly one byte (posted %s -> %s)' % (posted4, posted5))
check(queued5 == 0 and focus5 == 'surface 2',
      "keyboard: the window's owner - the console - drained the byte (queued %s -> %s)" % (queued4, queued5))
check('a' in between and 'aletheia>' not in between,
      "keyboard: the console ECHOED the virtio keystroke - the terminal window is the console's second surface (%r)" % (between[:40],))
# 5b - a whole command typed on the virtio keyboard runs at the same shell the serial line
#      drives: repair the line, type `help`, and the console answers on the serial log while
#      the terminal window's own last line shows the prompt again.
mark = len(log_text())
for k in ['h', 'e', 'l', 'p']:
    press([k])
press(['ret'])
wait_for('commands:', timeout=30)
answered = 'commands:' in log_text()[mark:]
check(answered, 'keyboard: `help` typed on the virtio keyboard was answered by the console')
chunk, facts6 = run_input()
term_line = TERM_RE.search(chunk)
win = WIN_RE.search(chunk)
# The grid's last non-blank line at the moment `input` composes its answer is the prompt line
# with the command just typed - `aletheia> input` - and the line count grows with every readout.
lines6 = int(term_line.group(1)) if term_line else None
check(term_line is not None and term_line.group(2) == 'aletheia> input',
      'terminal: the window shows the console (last line %r)' % (term_line.group(2) if term_line else None,))
chunk, _f = run_input()
term_again = TERM_RE.search(chunk)
check(term_again is not None and lines6 is not None and int(term_again.group(1)) > lines6,
      'terminal: the grid grows with the console (%s -> %s lines)' % (lines6, term_again.group(1) if term_again else None))
check(win is not None and (int(win.group(1)), int(win.group(2))) == (300, 60),
      'window: placed where the desktop put it (%r)' % ((win.group(1), win.group(2)) if win else None,))

# 5c - drag by the title band: press on the band, move, release; the window follows the
#      pointer by exactly the pointer's delta and keeps focus.
PRESS, DROP = (20480, 8740), (21504, 12288)     # (400, 64) in the title band -> (420, 90)
abs_move(*PRESS)
pointer([{'type': 'btn', 'data': {'button': 'left', 'down': True}}])
abs_move(*DROP)
pointer([{'type': 'btn', 'data': {'button': 'left', 'down': False}}])
chunk, facts7 = run_input()
win2 = WIN_RE.search(chunk)
# Expected from the SAME axis mapping the kernel uses, never a hardcoded pixel: the window moves
# by exactly the mapped pointer delta.
expect = (300 + map_axis(DROP[0], SCAN_W) - map_axis(PRESS[0], SCAN_W),
          60 + map_axis(DROP[1], SCAN_H) - map_axis(PRESS[1], SCAN_H))
check(win2 is not None and (int(win2.group(1)), int(win2.group(2))) == expect and facts7[4] == 'surface 2',
      'window: a drag by the title band moved it by the mapped pointer delta (%r, expected %r)' % ((win2.group(1), win2.group(2)) if win2 else None, expect))

# 5d - THE SECOND WINDOW (ADR-084): the desktop came up with two managed windows, and a click
#      on the monitor window takes focus away from the terminal. Focus is a routing decision:
#      from here a keystroke lands in the monitor's queue and the console never sees it.
def axis_for(px, span):
    """The tablet value that maps to exactly pixel `px` under the kernel's own mapping."""
    v = (px * (AXIS_MAX + 1)) // span
    while map_axis(v, span) < px:
        v += 1
    assert map_axis(v, span) == px, (px, span, v, map_axis(v, span))
    return v

def goto(px, py):
    abs_move(axis_for(px, SCAN_W), axis_for(py, SCAN_H))

chunk, _f = run_input()
w0 = WINDOWS_RE.search(chunk)
check(w0 is not None and int(w0.group(1)) == 2 and int(w0.group(2)) == 0,
      'windows: the desktop came up with two managed windows (%r)' % ((w0.groups() if w0 else None),))
check('managed windows' in log_text(),
      'windows: the boot log names how many windows the manager holds')
goto(100, 170)                                   # the monitor window's client area
click()
chunk, (_p8, _d8, _r8, _c8, focus8, _q8) = run_input()
check(focus8 == 'surface 3', 'windows: a click on the second window focuses it (%r)' % (focus8,))
mark = len(log_text())
press(['b'])
time.sleep(0.4)
chunk, (_p9, _d9, _r9, _c9, focus9, queued9) = run_input()
check(queued9 >= 1 and focus9 == 'surface 3',
      'windows: the keystroke queued behind the focused monitor, unread (queued %r)' % (queued9,))
check('b' not in log_text()[mark:len(log_text())].split('input')[0],
      'windows: the console never saw the keystroke that went to the other window')

# 5e - THE CLOSE BOX: pressing it ends the window - its surface, its queue and its token die
#      together - and focus falls to the survivor, which is the console's terminal again.
goto(254, 144)                                   # the monitor's close box (top-right corner)
click()
chunk, (_pa, _da, _ra, _ca, focusa, _qa) = run_input()
wa = WINDOWS_RE.search(chunk)
check(wa is not None and int(wa.group(1)) == 1 and int(wa.group(2)) == 1,
      'windows: the close box closed the window (%r)' % ((wa.groups() if wa else None),))
check(focusa == 'surface 2',
      'windows: focus fell to the surviving window, not to nobody (%r)' % (focusa,))

# 5f - the console is live again on its own window: a keystroke typed now is echoed by the
#      shell, which proves the survivor is focused in fact and not only in the readout.
mark = len(log_text())
press(['c'])
time.sleep(0.5)
between = log_text()[mark:]
press(['backspace'])
time.sleep(0.3)
check('c' in between,
      'windows: after the close, the keyboard types at the surviving terminal again (%r)' % (between[:40],))

# 6 — quiet: with nothing happening, the ledger holds still.
chunk, (posted6, dropped6, refused6, cursor6, _f6, queued6) = run_input()
time.sleep(1.0)
chunk, (posted7, dropped7, refused7, cursor7, _f7, queued7) = run_input()
check(posted6 == posted7 and dropped6 == dropped7 and refused6 == refused7 and queued6 == queued7,
      'quiet: with no events the ledger holds still (%s/%s/%s vs %s/%s/%s)' %
      (posted6, dropped6, queued6, posted7, dropped7, queued7))
check(cursor6 == cursor7,
      'quiet: the cursor does not drift (%r)' % (cursor7,))

print()
if fails:
    print('FAILED: ' + '; '.join(fails))
    print(log_text()[-4000:])
    sys.exit(1)
print('real input devices route through the session: pointer -> cursor, click -> focus, key -> queue')
sys.exit(0)
PYEOF
rc=$?

kill -9 "$QEMU_PID" 2>/dev/null
wait "$QEMU_PID" 2>/dev/null

hr
if [ "$rc" -eq 0 ]; then
  echo "VINPUT-E2E: PASS"
else
  echo "VINPUT-E2E: FAIL"
fi
exit "$rc"
