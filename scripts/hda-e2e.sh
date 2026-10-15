#!/usr/bin/env bash
# The tone is HEARD, not only counted (REQ-DRV-017, ADR-258).
#
# The boot gates prove the HD Audio driver resets the controller, walks the codec, configures the
# output path and runs the output stream until its buffers complete. This rung proves the bytes the
# stream carried were SOUND: the aarch64 machine boots with QEMU's `wav` audio backend behind the
# intel-hda + hda-output pair, the boot suite plays its 750 Hz triangle tone (64 samples a period
# at 48 kHz), and the host reads the WAV file QEMU wrote: it must hold a signal well above silence
# whose zero-crossing rate is 1500/s within 10 %. A driver that ran the stream over zeroed or
# misaddressed buffers would pass the boot gate and fail here.
#
# x86-64 is measured the same way (ADR-262): the full x86-64 boot gate (scripts/vm-e2e-x86.sh) runs
# with the HDA output recorded to a WAV file instead of discarded, so the tone the x86-64 boot plays
# at 00:08.0 is held to the same level and frequency checks.
#
# Exit 0 PASS. SKIP (exit 0, never silent pass) when the host lacks qemu-system-aarch64/python3;
# the x86-64 leg SKIPs by name when the host lacks qemu-system-x86_64 + mtools + OVMF.
set -uo pipefail

if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
A64="$ROOT/kernel"
T="$A64/target"
LOG="$T/hda-e2e.log"
WAV="$T/hda-e2e.wav"

hr() { printf '========================================================================\n'; }

PY="$(command -v python3 || command -v python)"
if ! command -v qemu-system-aarch64 >/dev/null 2>&1 || [ -z "$PY" ]; then
  echo "SKIP: needs qemu-system-aarch64 + python3 (this rung did NOT run)"
  echo "HDA-E2E: SKIP"
  exit 0
fi

hr; echo "==> building the aarch64 kernel"; hr
( cd "$A64" && cargo build ) || { echo "FAIL: build"; echo "HDA-E2E: FAIL"; exit 1; }
mkdir -p "$T"
rm -f "$LOG" "$WAV"
for d in scratch persist pci; do dd if=/dev/zero of="$T/hda-$d.img" bs=1048576 count=1 2>/dev/null; done
# A direct -kernel ELF boot gets no DTB pointer; the machine declares itself over fw_cfg (ADR-074).
MACHINE=(-machine virt,iommu=smmuv3,highmem-ecam=off,gic-version=2 -global arm-smmuv3.stage=2
  -cpu cortex-a72 -smp 4 -m 128M)
qemu-system-aarch64 "${MACHINE[@]}" -nographic -machine dumpdtb="$T/hda-dtb-raw.bin" >/dev/null 2>&1
set -- $(od -An -tu1 -j4 -N4 "$T/hda-dtb-raw.bin")
head -c $(( $1 << 24 | $2 << 16 | $3 << 8 | $4 )) "$T/hda-dtb-raw.bin" > "$T/hda-dtb.bin"

hr; echo "==> booting with intel-hda + hda-output recording to a WAV file"; hr
perl -e 'alarm 300; exec @ARGV or die' qemu-system-aarch64 "${MACHINE[@]}" -display none \
  -semihosting-config enable=on,target=native \
  -kernel "$A64/target/aarch64-unknown-none-softfloat/debug/aletheia-kernel" \
  -global virtio-mmio.force-legacy=false \
  -drive "if=none,format=raw,file=$T/hda-scratch.img,id=blk0" -device virtio-blk-device,drive=blk0 \
  -drive "if=none,format=raw,file=$T/hda-persist.img,id=blk1" -device virtio-blk-device,drive=blk1 \
  -drive "if=none,format=raw,file=$T/hda-pci.img,id=pciblk0" \
  -device virtio-blk-pci,disable-legacy=on,iommu_platform=on,drive=pciblk0 \
  -audiodev "wav,id=snd0,path=$WAV" -device intel-hda -device hda-output,audiodev=snd0 \
  -fw_cfg "name=opt/org.aletheia/dtb,file=$T/hda-dtb.bin" \
  -serial "file:$LOG" >/dev/null 2>&1
echo "  boot exit code: $?"

# measure LOG WAV: the boot played its tone (LOG) and the capture (WAV) is that tone.
measure() {
"$PY" - "$1" "$2" <<'PYEOF'
import struct, sys
log_path, wav_path = sys.argv[1], sys.argv[2]
fails = []
def check(ok, name):
    print(('  [pass] ' if ok else '  [FAIL] ') + name)
    if not ok:
        fails.append(name)

log = open(log_path, 'rb').read().decode('utf-8', 'replace')
check('ALL 6 HDA INVARIANTS HOLD' in log, 'hda: the boot suite played its tone')
# The guest ends the run through semihosting, so QEMU never rewrites the RIFF and data sizes it
# left at zero; the format fields are read from the canonical 44-byte header and the data runs to
# the end of the file.
blob = open(wav_path, 'rb').read()
if len(blob) < 44 or blob[0:4] != b'RIFF' or blob[8:12] != b'WAVE' or blob[36:40] != b'data':
    print('  [FAIL] the capture is a canonical WAV file'); sys.exit(1)
ch, rate = struct.unpack_from('<HI', blob, 22)
width = struct.unpack_from('<H', blob, 34)[0] // 8
raw = blob[44:]
check(width == 2, 'wav: 16-bit samples (%d bytes)' % width)
left = [struct.unpack_from('<h', raw, i)[0] for i in range(0, len(raw) - 1, 2 * ch)]
loud = [i for i, s in enumerate(left) if abs(s) > 200]
if not loud:
    check(False, 'wav: the capture holds a signal above silence'); print('FAILED'); sys.exit(1)
span = left[loud[0]:loud[-1] + 1]
secs = len(span) / rate
rms = (sum(s * s for s in span) / len(span)) ** 0.5
crossings = sum(1 for a, b in zip(span, span[1:]) if (a < 0) != (b < 0))
zcr = crossings / secs if secs else 0
print('  [info] %d Hz, %d ch: %.3f s of signal, RMS %.0f, %d crossings = %.0f/s'
      % (rate, ch, secs, rms, crossings, zcr))
check(secs >= 0.05, 'wav: at least 50 ms of tone')
check(rms > 500, 'wav: the signal is well above silence')
check(abs(zcr - 1500) <= 150, 'wav: 750 Hz (1500 zero crossings a second, within 10 %)')
print()
if fails:
    print('FAILED: ' + '; '.join(fails)); sys.exit(1)
print('the driver\'s buffers reached the speaker as a 750 Hz tone')
PYEOF
}
measure "$LOG" "$WAV"
a_rc=$?

hr; echo "==> x86-64: the full boot gate with the HDA output recorded to a WAV file"; hr
X86LOG="$T/hda-e2e-x86.log"
X86WAV="$T/hda-e2e-x86.wav"
rm -f "$X86LOG" "$X86WAV"
have_ovmf=0
for c in "${OVMF_CODE:-}" /opt/homebrew/share/qemu/edk2-x86_64-code.fd /usr/share/OVMF/OVMF_CODE_4M.fd \
         /usr/share/OVMF/OVMF_CODE.fd /usr/share/edk2/x64/OVMF_CODE.4m.fd; do
  [ -n "$c" ] && [ -f "$c" ] && { have_ovmf=1; break; }
done
if command -v qemu-system-x86_64 >/dev/null 2>&1 && command -v mformat >/dev/null 2>&1 && [ "$have_ovmf" = "1" ]; then
  HDA_AUDIODEV="wav,path=$X86WAV" bash "$ROOT/scripts/vm-e2e-x86.sh" > "$X86LOG" 2>&1; g_rc=$?
  grep -E 'VM-E2E-X86|SMOKE TEST' "$X86LOG"
  if [ "$g_rc" -eq 0 ]; then measure "$X86LOG" "$X86WAV"; x_rc=$?; else x_rc=1; fi
  x_res="$([ "$x_rc" -eq 0 ] && echo PASS || echo FAIL)"
else
  echo "  x86-64 leg unavailable on this host (needs qemu-system-x86_64 + mtools + OVMF) — SKIPPED (never a silent pass)."
  x_rc=0; x_res="SKIP"
fi

hr
echo "  aarch64 : $([ "$a_rc" -eq 0 ] && echo PASS || echo FAIL)"
echo "  x86-64  : $x_res"
rc=$(( a_rc != 0 || x_rc != 0 ))
if [ "$rc" -eq 0 ]; then echo "HDA-E2E: PASS"; else echo "HDA-E2E: FAIL"; fi
exit "$rc"
