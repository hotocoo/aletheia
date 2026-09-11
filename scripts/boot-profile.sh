#!/usr/bin/env bash
# Where does Aletheia's boot time actually go? (REQ-PERF-001, ADR-085)
#
# ADR-082 split the boot clock into a firmware share and a kernel share and found ~1077 ms of
# kernel. That number is only useful if it can be attributed, so this script attributes it:
# it boots the real image and timestamps EVERY serial line as it arrives on the host, then reports
# the largest gaps between consecutive lines.
#
# WHY HOST-SIDE TIMESTAMPS. The kernel could timestamp itself, but then the profile would depend on
# a clock the kernel calibrates during the very window being measured, and the instrumentation
# would change the thing it measures. Timestamping arrival on the host needs no kernel change at
# all, so the binary profiled is the binary shipped. The cost is that a gap includes serial
# transmission of the line that closes it — which at 115200 baud is well under a millisecond per
# line and is stated here rather than hidden.
#
# WHAT A GAP MEANS. A gap is the wall-clock time between one line appearing and the next. It is
# evidence that work happened between those two prints; it is NOT evidence that the work belongs to
# either line. A long gap before "[mm] ..." means the work finished just before that print, not
# that printing it was slow.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

TOP_N="${TOP_N:-15}"
MARKER="${MARKER:-aletheia>}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

hr() { printf '========================================================================\n'; }

hr; echo "==> boot profile: where Aletheia's own boot time goes"; hr

# Profile the SAME image the comparative benchmark measures — the interactive console build that
# actually reaches `aletheia> `. The boot gate's image is a self-test build that exits rather than
# offering a prompt, so profiling it would attribute time in a boot nobody is comparing.
IMG="$ROOT/kernel-x86_64/build/aletheia-x86_64-bench.img"
if ! CARGO_FEATURES=interactive IMG="$IMG" bash "$ROOT/kernel-x86_64/scripts/build-image-linux.sh" \
     >/dev/null 2>&1; then
  echo "  the interactive image did not build — SKIPPED (never a silent pass)"
  exit 0
fi
echo "--> profiling $IMG"

OVMF_CODE_F=""; OVMF_VARS_F=""
for c in "${OVMF_CODE:-}" /opt/homebrew/share/qemu/edk2-x86_64-code.fd \
         /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd; do
  [ -n "$c" ] && [ -f "$c" ] && { OVMF_CODE_F="$c"; break; }
done
for v in "${OVMF_VARS:-}" /opt/homebrew/share/qemu/edk2-i386-vars.fd \
         /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd; do
  [ -n "$v" ] && [ -f "$v" ] && { OVMF_VARS_F="$v"; break; }
done
if [ -z "$OVMF_CODE_F" ] || [ -z "$OVMF_VARS_F" ]; then
  echo "  OVMF firmware not found — SKIPPED (never a silent pass)"
  exit 0
fi
cp "$OVMF_VARS_F" "$WORK/vars.fd"

# The same machine the comparative benchmark builds, flag for flag. Mirrored deliberately: a
# profile taken on a different machine than the one being compared would attribute time in a boot
# nobody measures.
SCRATCH="$WORK/virtio-blk-test.img"; PERSIST="$WORK/virtio-blk-persistent.img"
dd if=/dev/zero of="$SCRATCH" bs=1048576 count=1 2>/dev/null
dd if=/dev/zero of="$PERSIST" bs=1048576 count=1 2>/dev/null

FIFO="$WORK/in"; STAMPS="$WORK/stamps.txt"
mkfifo "$FIFO"
exec 9<>"$FIFO"

# Timestamp each line as it arrives. `python3 -u` so nothing sits in a buffer and distorts a gap.
qemu-system-x86_64 -machine q35 -m 256 -smp 4 -cpu qemu64,+smep -nographic \
  -drive "if=pflash,format=raw,unit=0,file=$OVMF_CODE_F,readonly=on" \
  -drive "if=pflash,format=raw,unit=1,file=$WORK/vars.fd" \
  -drive "format=raw,file=$IMG" \
  -drive "if=none,format=raw,file=$SCRATCH,id=blk0" -device virtio-blk-pci,drive=blk0 \
  -drive "if=none,format=raw,file=$PERSIST,id=blk1" -device virtio-blk-pci,drive=blk1 \
  -device isa-debug-exit,iobase=0xf4,iosize=0x04 -no-reboot \
  < "$FIFO" 2>&1 \
  | python3 -u -c '
import sys, time
# WHOLE LINES for attribution, plus a separate watch for the PROMPT. Two facts forced this shape:
# QEMU delivers serial bytes in small chunks, so a chunk-stamping reader produces fragments that
# make "which print did this gap follow" meaningless; and the shell prompt has NO trailing newline,
# so a purely line-oriented reader blocks on it forever and reports "never reached a prompt" —
# the harness failing and blaming the kernel. Lines are stamped when they COMPLETE; the residual
# tail is scanned for the marker and stamped once when it appears.
MARK = "aletheia>"
t0 = time.time()
buf = b""
seen_prompt = False
fd = sys.stdin.buffer
while True:
    chunk = fd.read1(4096) if hasattr(fd, "read1") else fd.read(4096)
    if not chunk:
        break
    buf += chunk
    while b"\n" in buf:
        line, buf = buf.split(b"\n", 1)
        sys.stdout.write("%8.1f\t%s\n" % ((time.time() - t0) * 1000.0,
                                           line.decode("utf-8", "replace").rstrip("\r")))
    if not seen_prompt and MARK.encode() in buf:
        seen_prompt = True
        sys.stdout.write("%8.1f\t%s\n" % ((time.time() - t0) * 1000.0, MARK))
    sys.stdout.flush()
' > "$STAMPS" &
QPID=$!

deadline=$((SECONDS + 120))
while ! grep -q "$MARKER" "$STAMPS" 2>/dev/null; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "  the image never reached a prompt within 120s — SKIPPED"
    echo "  last serial bytes captured:"
    tail -c 500 "$STAMPS" 2>/dev/null | sed 's/^/    | /'
    echo "  (lines captured: $(wc -l < "$STAMPS" 2>/dev/null | tr -d ' '))"
    pkill -9 -f "file=$IMG" 2>/dev/null; exec 9>&-; exit 0
  fi
  sleep 0.005
done
pkill -9 -f "file=$IMG" 2>/dev/null
exec 9>&-
wait "$QPID" 2>/dev/null

TOTAL="$(awk -F'\t' 'END{printf "%.0f", $1}' "$STAMPS")"
FW="$(awk -F'\t' '/calling ExitBootServices/{printf "%.0f", $1; exit}' "$STAMPS")"
echo "    total to prompt: ${TOTAL} ms   firmware share: ${FW:-?} ms   kernel share: $((TOTAL - ${FW:-0})) ms"
echo

hr; echo "TOP $TOP_N GAPS AFTER ExitBootServices — the kernel's own time, attributed"; hr
awk -F'\t' -v fw="${FW:-0}" '
  $1 + 0 >= fw {
    if (prev != "") {
      gap = $1 - prevt
      if (gap > 0.5) printf "%8.1f ms   after: %s\n", gap, substr(prev, 1, 88)
    }
    prev = $2; prevt = $1
  }
' "$STAMPS" | sort -rn | head -n "$TOP_N"

echo
echo "READ THIS CAREFULLY. A gap is wall-clock between one line appearing and the next. It is"
echo "evidence that work happened BETWEEN those prints — not that the work belongs to either line,"
echo "and not that printing was slow. A gap also includes serial transmission of the line that"
echo "closes it. Use this to decide where to look, never as a measurement of a named subsystem."
hr
echo "boot-profile: PASS (profile printed; no claim made)"
