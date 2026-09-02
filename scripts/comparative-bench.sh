#!/usr/bin/env bash
# Aletheia against a real Linux kernel, on the same host, under the same emulator (REQ-PERF-001,
# ADR-056).
#
# WHY THIS EXISTS, AND WHAT IT REFUSES TO DO.
#
# "Faster than Linux" is a claim almost nobody who makes it has measured, because almost nobody who
# makes it is running the two on the same substrate. `scripts/linux_pipe_bench.sh` already says so
# about IPC: Aletheia's kernel numbers come out of QEMU TCG emulation and a Linux container's come
# out of near-native execution, and comparing those wall-clocks would be comparing emulators.
#
# This gate removes that objection instead of restating it. BOTH systems boot under the SAME
# `qemu-system-x86_64`, on the same host, with the same `-machine`, `-m`, `-smp` and `-cpu`, in the
# same TCG emulation mode, to the same end state: an interactive shell on ttyS0 that is sitting there
# waiting for somebody to type. Everything below is measured across that line.
#
# It also reports what Aletheia LOSES. A benchmark that only prints the columns its author wins is
# marketing, and this repository's `docs/MATURITY.md` exists precisely to stop that.
#
# WHAT IS AND IS NOT BEING COMPARED. Linux 6.12-lts is a general-purpose kernel with drivers for
# tens of thousands of devices, filesystems, namespaces, a network stack and thirty years of
# hardware workarounds. Aletheia's console kernel is a microkernel with one filesystem, one block
# driver, one network driver and a shell. **These are not the same product**, and a boot-time or
# image-size number that flatters the smaller one is measuring the size difference, not a design
# win. What IS a design claim, and is stated as one, is where the size difference COMES FROM: an
# object with no ambient authority, a capability check on the syscall path, and a `no_std` core with
# no allocator in the boot path. Those are choices; the numbers below are their consequences.
#
# RUNNING IT:  ./scripts/comparative-bench.sh
# The Linux leg needs Docker (to build a busybox initramfs) and network access (to fetch the kernel).
# It SKIPs — never silently passes, and never quietly drops the comparison — when either is missing.
#
# THE TYPED-WORKLOAD LEG (REQ-PERF-002): boot and idle say how a machine WAITS; this leg says how
# it ANSWERS. After the idle sampling, still alive, each guest receives the IDENTICAL scripted
# session over its serial console: WORKLOAD_OPS 'echo WL-<label>-<i>' lines, one at a time, each
# held until THAT op's unique token comes back as OUTPUT (anchored '^WL-', so the terminal's own
# input echo of 'echo WL-...' can never satisfy the wait). Wall-clock across all N round-trips is
# reported as ms/op beside the other columns. This is an END-TO-END interactive-path measurement —
# tty discipline or kernel input ring, line editor, dispatcher, output formatting, serial
# transmission — under the same emulator, driven by the same script, and it is a small STRESS test
# too: the input path must take N paced lines with nothing dropped (Aletheia's 'mem' command can
# prove the drop counter stayed at zero afterward; see the honesty notes at the bottom).
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

WORK="$(mktemp -d)"
ALPINE_VER="${ALPINE_VER:-v3.21}"
KERNEL_URL="${KERNEL_URL:-https://dl-cdn.alpinelinux.org/alpine/$ALPINE_VER/releases/x86_64/netboot/vmlinuz-lts}"
IDLE_SAMPLES="${IDLE_SAMPLES:-6}"
IDLE_INTERVAL="${IDLE_INTERVAL:-2}"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-180}"
# Typed-workload sizing: 25 paced round-trips is seconds of wall clock and enough to see the
# per-op cost rise above timer noise; set WORKLOAD_OPS=0 to skip the leg entirely.
WORKLOAD_OPS="${WORKLOAD_OPS:-25}"
# One boot is not a measurement; see boot_median. Three is enough to see a distribution and cheap
# enough to run on every push.
BOOT_SAMPLES="${BOOT_SAMPLES:-3}"

fail=0
hr() { printf '========================================================================\n'; }
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

# Results, filled per leg.
AL_BOOT_MS=""; AL_IDLE=""; AL_BYTES=""; AL_WL_MS=""
LX_BOOT_MS=""; LX_IDLE=""; LX_BYTES=""; LX_STATUS="SKIP"; LX_WL_MS=""

# ---------------------------------------------------------------------------------------------
# THE TYPED-WORKLOAD LEG. Drive WORKLOAD_OPS echo round-trips into a LIVE guest through its
# serial console, one at a time, each held until that op's unique token comes back as OUTPUT.
# The wait anchors on '^WL-' so the terminal's own INPUT ECHO of 'echo WL-...' can never satisfy
# it — we measure the guest's answer, not our own typing. Wall-clock across all ops is reported
# as ms/op. Both guests get byte-identical treatment: same emulator, same pacing, same protocol,
# same judge (this loop). Redox is excluded from THIS leg only because its image boots to a login
# prompt whose credentials this script refuses to guess on somebody else's OS.
#
# The fifo is already held open on fd 9 by the caller; writing goes to the same pipe the guest
# reads. Polling granularity is 20 ms — far below one round-trip under TCG, so it prices in no
# meaningful share of the result.
typed_workload() {
  local label="$1" log="$2" t0 t1 i deadline
  [ "$WORKLOAD_OPS" -gt 0 ] || return 0
  t0="$(python3 -c 'import time;print(int(time.time()*1000))')"
  for i in $(seq 1 "$WORKLOAD_OPS"); do
    printf 'echo WL-%s-%s\n' "$label" "$i" >&9
    deadline=$((SECONDS + 60))
    until grep -qE "^WL-$label-$i\r*$" "$log"; do
      if [ "$SECONDS" -ge "$deadline" ]; then
        echo "    FAIL [$label] workload op $i was never answered by the guest"
        # Show what the guest DID say, bytes visible: a harness failure and a guest failure look
        # identical from the verdict line alone, and this leg has failed on a runner nobody could
        # watch (both legs at op 1, which indicts the wire, not either guest).
        echo "    --- last 400 bytes of the guest log (od -c) ---"
        tail -c 400 "$log" 2>/dev/null | od -c | tail -12 | sed 's/^/    /'
        return 1
      fi
      sleep 0.005
    done
  done
  t1="$(python3 -c 'import time;print(int(time.time()*1000))')"
  WORKLOAD_MS=$((t1 - t0))
  printf '    typed workload: %s echo round-trips in %s ms => %s ms/op end-to-end\n' \
    "$WORKLOAD_OPS" "$WORKLOAD_MS" "$((WORKLOAD_MS / WORKLOAD_OPS))"
  return 0
}

# ---------------------------------------------------------------------------------------------
# Boot something, wait for the string that means "there is a prompt", and then measure the host CPU
# the guest costs while NOBODY IS TYPING.
#
# The idle number is the one this script was written for. A guest parked at a prompt should cost
# approximately nothing: the work it has to do is zero, and any CPU it burns is CPU it is taking
# from whatever else the machine is doing -- which, for Aletheia, is the model that is deciding what
# to type next. Measured on the QEMU process because that is where a spinning guest actually shows
# up; a guest that busy-waits reports itself as idle from the inside.
#
# stdin is held open on a FIFO for the whole measurement. Without it QEMU sees EOF, the console reads
# end-of-input, and the thing being measured stops existing halfway through measuring it.
# ---------------------------------------------------------------------------------------------
# Boot the same guest BOOT_SAMPLES times and report the MEDIAN time to a prompt.
#
# One boot is not a measurement, and the first version of this script found that out the honest way:
# it reported Aletheia 4068 ms against Linux 2053 ms, and the commentary was written around Linux
# winning. Run again on the same host, same binaries: 3080 against 3070. The two distributions
# overlap, and a single sample had been read as a result. Everything under `-accel tcg` on a
# workstation that is also running a browser and a language model moves like this, which is exactly
# why the idle number was a median from the start.
boot_median() {
  local label="$1" marker="$2"; shift 2
  local -a samples=()
  local i
  for i in $(seq 1 "$BOOT_SAMPLES"); do
    boot_and_measure "$label-$i" "$marker" "$@" || return 1
    samples+=("$BOOT_MS")
    # Only the last run's idle number is kept: idle is idle, and it is already a median over samples.
  done
  BOOT_MS="$(printf '%s\n' "${samples[@]}" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}')"
  BOOT_ALL="$(printf '%s ' "${samples[@]}")"
  printf '    boot to a prompt: median %s ms over %s runs (%s)\n' "$BOOT_MS" "$BOOT_SAMPLES" "$BOOT_ALL"
  return 0
}

boot_and_measure() {
  local label="$1" marker="$2"; shift 2
  local -a argv=("$@")
  local fifo="$WORK/$label.in" log="$WORK/$label.log"
  mkfifo "$fifo"
  exec 9<>"$fifo"

  local t0 t1
  t0="$(python3 -c 'import time;print(int(time.time()*1000))')"
  "${argv[@]}" < "$fifo" > "$log" 2>&1 &
  local pid=$!

  local waited=0
  while ! grep -q "$marker" "$log" 2>/dev/null; do
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "  FAIL [$label] the guest exited before it reached a prompt"
      sed -n '$p' "$log"; exec 9>&-; return 1
    fi
    sleep 1; waited=$((waited + 1))
    if [ "$waited" -ge "$BOOT_TIMEOUT" ]; then
      echo "  FAIL [$label] no prompt within ${BOOT_TIMEOUT}s"
      kill -9 "$pid" 2>/dev/null; exec 9>&-; return 1
    fi
  done
  t1="$(python3 -c 'import time;print(int(time.time()*1000))')"
  BOOT_MS=$((t1 - t0))
  echo "    booted to a prompt in ${BOOT_MS} ms"

  # The first sample is discarded: it catches the tail of boot rather than the idle the rest of the
  # samples are measuring, and including it would report a busy number for a quiet machine.
  local -a samples=()
  local i cpu
  for i in $(seq 1 $((IDLE_SAMPLES + 1))); do
    cpu="$(ps -o %cpu= -p "$pid" 2>/dev/null | tr -d ' ')"
    [ -z "$cpu" ] && break
    [ "$i" -gt 1 ] && samples+=("$cpu")
    sleep "$IDLE_INTERVAL"
  done
  # The guest stays ALIVE for the typed-workload leg: boot and idle say how a machine waits,
  # the workload says how it ANSWERS. Only after it does the guest get put down.
  typed_workload "$label" "$log" || { kill -9 "$pid" 2>/dev/null; exec 9>&-; return 1; }
  kill -9 "$pid" 2>/dev/null
  exec 9>&-

  if [ "${#samples[@]}" -eq 0 ]; then
    echo "  FAIL [$label] the guest died during the idle measurement"; return 1
  fi
  IDLE_CPU="$(printf '%s\n' "${samples[@]}" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}')"
  printf '    idle host CPU while nobody is typing: %s%% (median of %s samples: %s)\n' \
    "$IDLE_CPU" "${#samples[@]}" "$(printf '%s ' "${samples[@]}")"
  return 0
}

# ---------------------------------------------------------------------------------------------
hr; echo "==> Aletheia (x86-64, UEFI, interactive console)"; hr

IMG="$ROOT/kernel-x86_64/build/aletheia-x86_64-bench.img"
CARGO_FEATURES=interactive IMG="$IMG" bash "$ROOT/kernel-x86_64/scripts/build-image-linux.sh" \
  >/dev/null 2>&1 || { echo "FAIL: the Aletheia image did not build"; exit 1; }

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
  echo "FAIL: no OVMF on this host — the Aletheia leg cannot run"; exit 1
fi

# What you must ship to reach a prompt. For Aletheia that is the EFI executable, NOT the 64 MiB FAT
# volume it is installed onto -- the volume is mostly empty space and counting it would be measuring
# a `dd` argument. The Linux column is measured the same way: the kernel plus the initramfs it
# cannot boot without, and nothing else.
AL_EFI="$(find "$ROOT/kernel-x86_64/target" -name 'aletheia-kernel-x86_64.efi' -newermt '-1 hour' 2>/dev/null | head -1)"
[ -z "$AL_EFI" ] && AL_EFI="$(find "$ROOT/kernel-x86_64/target" -name '*.efi' 2>/dev/null | head -1)"
AL_BYTES="$(wc -c < "$AL_EFI" | tr -d ' ')"
echo "    bootable payload: $(basename "$AL_EFI") — $AL_BYTES bytes"

cp "$OVMF_VARS_F" "$WORK/al-vars.fd"
dd if=/dev/zero of="$WORK/al-s.img" bs=1048576 count=1 2>/dev/null
dd if=/dev/zero of="$WORK/al-p.img" bs=1048576 count=1 2>/dev/null

if boot_median aletheia "aletheia> " \
    qemu-system-x86_64 -machine q35 -m 256 -smp 4 -cpu qemu64,+smep -display none -serial stdio -monitor none \
    -drive "if=pflash,format=raw,unit=0,file=$OVMF_CODE_F,readonly=on" \
    -drive "if=pflash,format=raw,unit=1,file=$WORK/al-vars.fd" \
    -drive "format=raw,file=$IMG" \
    -drive "if=none,format=raw,file=$WORK/al-s.img,id=blk0" -device virtio-blk-pci,drive=blk0 \
    -drive "if=none,format=raw,file=$WORK/al-p.img,id=blk1" -device virtio-blk-pci,drive=blk1 \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 -no-reboot; then
  AL_BOOT_MS="$BOOT_MS"; AL_IDLE="$IDLE_CPU"; AL_WL_MS="$WORKLOAD_MS"
else
  fail=1
fi

# ---------------------------------------------------------------------------------------------
hr; echo "==> Linux (same host, same qemu-system-x86_64, same TCG, same -m/-smp/-cpu)"; hr

if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "  Linux leg needs Docker to build the initramfs — SKIPPED (never a silent pass)."
elif ! curl -sI --max-time 20 "$KERNEL_URL" >/dev/null 2>&1; then
  echo "  Linux leg needs network access to fetch the kernel — SKIPPED (never a silent pass)."
else
  echo "--> building a busybox initramfs whose /init prints a marker and execs a shell"
  # The guest must end in the SAME state as Aletheia's: an interactive shell on the serial line,
  # blocked on input. A distro that boots into a service manager would be measuring the distro.
  docker run --rm -v "$WORK:/out" --platform linux/amd64 alpine:3.21 sh -c '
    apk add --no-cache busybox-static cpio >/dev/null 2>&1
    mkdir -p /ir/bin /ir/dev /ir/proc /ir/sys
    cp /bin/busybox.static /ir/bin/busybox
    for a in sh cat echo mount sleep; do ln -sf busybox /ir/bin/$a; done
    printf "#!/bin/busybox sh\n/bin/busybox mount -t proc proc /proc 2>/dev/null\n/bin/busybox mount -t sysfs sys /sys 2>/dev/null\n/bin/busybox echo LINUX-BENCH-PROMPT-READY\nexec /bin/busybox sh\n" > /ir/init
    chmod +x /ir/init
    cd /ir && find . | cpio -o -H newc 2>/dev/null | gzip -9 > /out/initramfs.gz
  ' >/dev/null 2>&1 || { echo "  FAIL: the initramfs did not build"; fail=1; }

  if [ -s "$WORK/initramfs.gz" ]; then
    echo "--> fetching $KERNEL_URL"
    if curl -sL --max-time 300 -o "$WORK/vmlinuz" "$KERNEL_URL" && [ -s "$WORK/vmlinuz" ]; then
      LX_BYTES=$(( $(wc -c < "$WORK/vmlinuz") + $(wc -c < "$WORK/initramfs.gz") ))
      echo "    bootable payload: vmlinuz + initramfs — $LX_BYTES bytes"
      echo "    kernel: $(file "$WORK/vmlinuz" | sed -n 's/.*version \([^ ]*\).*/\1/p')"
      if boot_median linux "LINUX-BENCH-PROMPT-READY" \
          qemu-system-x86_64 -machine q35 -m 256 -smp 4 -cpu qemu64 -display none -serial stdio -monitor none -no-reboot \
          -kernel "$WORK/vmlinuz" -initrd "$WORK/initramfs.gz" \
          -append "console=ttyS0 quiet rdinit=/init"; then
        LX_BOOT_MS="$BOOT_MS"; LX_IDLE="$IDLE_CPU"; LX_STATUS="OK"; LX_WL_MS="$WORKLOAD_MS"
      else
        fail=1
      fi
    else
      echo "  FAIL: the kernel did not download"; fail=1
    fi
  fi
fi

# ---------------------------------------------------------------------------------------------
# Redox — the other Rust operating system that ships a bootable x86-64 image, on the same emulator.
#
# Included because "compare against other Rust OSes" is otherwise a table of adjectives. Redox is
# the only one of the obvious set that can be DOWNLOADED and BOOTED without building a toolchain
# first, so it is the only one that gets a measured column; the rest are in the attribute table
# below, where nothing is claimed that was not read from their own documentation.
#
# OPT-IN, because the image is ~70 MB compressed and this script otherwise runs in CI on every push:
#   WITH_REDOX=1 ./scripts/comparative-bench.sh
# ---------------------------------------------------------------------------------------------
RX_BOOT_MS=""; RX_IDLE=""; RX_BYTES=""
if [ "${WITH_REDOX:-0}" = "1" ]; then
  hr; echo "==> Redox OS (same host, same emulator, same flags)"; hr
  RX_URL="${REDOX_URL:-https://static.redox-os.org/img/x86_64/redox_server_x86_64_2026-07-27_475_harddrive.img.zst}"
  if ! command -v zstd >/dev/null 2>&1; then
    echo "  Redox leg needs zstd to decompress the image — SKIPPED (never a silent pass)."
  elif ! curl -sL --max-time 900 -o "$WORK/redox.img.zst" "$RX_URL" || [ ! -s "$WORK/redox.img.zst" ]; then
    echo "  Redox image did not download — SKIPPED (never a silent pass)."
  else
    zstd -qdf "$WORK/redox.img.zst" -o "$WORK/redox.img" 2>/dev/null
    RX_BYTES="$(wc -c < "$WORK/redox.img" | tr -d ' ')"
    echo "    bootable payload: whole disk image — $RX_BYTES bytes"
    # Redox's own marker: it prints a login prompt on the serial console. Matched on `login:` rather
    # than a shell prompt because the image boots to a getty, and forcing it further would be
    # measuring how well this script can drive somebody else's OS.
    if boot_median redox "login:" \
        qemu-system-x86_64 -machine q35 -m 2048 -smp 4 -cpu qemu64 -display none -serial stdio -monitor none -no-reboot \
        -drive "format=raw,file=$WORK/redox.img"; then
      RX_BOOT_MS="$BOOT_MS"; RX_IDLE="$IDLE_CPU"
    else
      echo "  Redox did not reach a login prompt on this host — reported, not hidden."
    fi
  fi
else
  hr; echo "==> Redox OS — not run (set WITH_REDOX=1; the image is ~70 MB)"; hr
fi

# ---------------------------------------------------------------------------------------------
# The trusted computing base, counted rather than asserted. This is the column where the design
# difference is real and does not depend on an emulator at all: it is how much code has to be
# correct for the machine to be correct.
# ---------------------------------------------------------------------------------------------
hr; echo "==> trusted computing base, counted from source"; hr
KLOC="$(find "$ROOT/kernel-core/src" "$ROOT/kernel-x86_64/src" -name '*.rs' -exec cat {} + 2>/dev/null | wc -l | tr -d ' ')"
KUNSAFE="$(find "$ROOT/kernel-core/src" "$ROOT/kernel-x86_64/src" -name '*.rs' -exec grep -c 'unsafe' {} + 2>/dev/null | awk -F: '{s+=$NF} END{print s+0}')"
KCORE_UNSAFE="$(find "$ROOT/kernel-core/src" -name '*.rs' -exec grep -c 'unsafe' {} + 2>/dev/null | awk -F: '{s+=$NF} END{print s+0}')"
echo "    Aletheia x86-64 kernel + shared core: $KLOC lines of Rust"
echo "    occurrences of \`unsafe\`: $KUNSAFE total, of which $KCORE_UNSAFE are in the shared core"
echo "    (Linux 6.12 is roughly 40 million lines of C, essentially all of it privileged. That"
echo "     number is not measured here and is cited only for scale.)"

# ---------------------------------------------------------------------------------------------
hr; echo "RESULTS — same host, same emulator, same emulation mode, same end state"; hr
printf '%-28s | %-18s | %-18s | %-18s\n' "" "Aletheia (x86-64)" "Linux 6.12-lts" "Redox OS"
printf '%-28s-+-%-18s-+-%-18s-+-%-18s\n' "----------------------------" "------------------" "------------------" "------------------"
printf '%-28s | %-18s | %-18s | %-18s\n' "boot to a prompt" "${AL_BOOT_MS:-FAIL} ms" "${LX_BOOT_MS:-SKIP} ms" "${RX_BOOT_MS:-SKIP} ms"
printf '%-28s | %-18s | %-18s | %-18s\n' "idle host CPU at prompt" "${AL_IDLE:-FAIL} %" "${LX_IDLE:-SKIP} %" "${RX_IDLE:-SKIP} %"
printf '%-28s | %-18s | %-18s | %-18s\n' "bootable payload" "${AL_BYTES:-FAIL} B" "${LX_BYTES:-SKIP} B" "${RX_BYTES:-SKIP} B"
printf '%-28s | %-18s | %-18s | %-18s\n' "typed echo round-trip" "${AL_WL_MS:-FAIL} ms" "${LX_WL_MS:-SKIP} ms" "n/a (login)"
printf '%-28s | %-18s | %-18s | %-18s\n' "privileged lines of code" "$KLOC (Rust)" "~40M (C, cited)" "n/a"
hr
cat <<'RUSTOS'
OTHER RUST OPERATING SYSTEMS — attributes, not numbers.

Redox is the only one of these that ships a bootable x86-64 image you can download and run, which is
why it is the only one with a measured column above (and only when WITH_REDOX=1). For the rest, a
number would have to come from somebody else's benchmark on somebody else's hardware, which is the
exact thing this script exists to refuse. What is comparable without running them is what they have
DECIDED, and that is stated here with no claim of superiority attached:

  Redox      microkernel, Rust, capability-ish (scheme/URL namespaces), POSIX-compatible userspace,
             self-hosting, x86-64/aarch64/riscv64. Much further along than Aletheia in userspace.
  seL4       microkernel, C, capability-based, and FORMALLY VERIFIED — functional correctness proved
             against its spec on selected configurations. Nothing in Aletheia is verified in that
             sense, and no amount of testing is the same claim.
  Theseus    Rust, "intralingual" design: safety enforced by the language and a single address space
             rather than by hardware boundaries. A different bet from Aletheia's, which uses the MMU
             and ring 3 and does not assume the compiler is the only thing between tasks.
  Hubris     Rust, memory-protected, no dynamic allocation, statically-defined task set at build
             time. Deployed in real hardware. A deliberately smaller problem than a general OS.
  Redshirt / rust-vmm / others — components rather than comparable systems.

WHERE ALETHEIA IS ACTUALLY DIFFERENT, stated as a design claim rather than a benchmark result:
the capability check and the intent->action pipeline are the SAME path for a human at the console and
for a model driving it (ADR-053/054/055). The model does not get an API around the OS; it gets the
operator's surface with the operator's authority, validated against the kernel's own command table,
and it cannot type a line a person could not have typed. Whether that is worth anything is not
something a boot-time number can answer.
RUSTOS
cat <<'NOTE'
HOW TO READ THIS.

  boot to a prompt              NO WINNER IS CLAIMED, and the history of this line is the reason.
                                The text here first predicted Aletheia would win; the first run said
                                Linux, 2053 ms against 4068; the second run on the same binaries said
                                3070 against 3080. One sample had been read as a result, twice, in
                                opposite directions. It is now a median over BOOT_SAMPLES runs, and
                                the individual samples are printed so the spread is visible rather
                                than hidden behind the median. Under TCG on a workstation that is
                                also running other things, treat a difference smaller than the spread
                                as no difference.

                                One structural asymmetry IS real and is Aletheia's to own: it boots
                                through OVMF, a full UEFI firmware implementation, while the Linux
                                leg is loaded directly by QEMU with `-kernel` and skips firmware
                                entirely. That is a boot-PATH difference, not evidence about either
                                kernel's speed.

  bootable payload              Aletheia wins by a wide margin, and the win is mostly the size
                                difference rather than a design victory: Linux ships drivers for
                                hardware Aletheia has never heard of. Reported because it is true
                                and measured, NOT because it is a fair fight.

  idle CPU at the prompt        A fair comparison, and the one worth having: both guests are doing
                                the same amount of work (none), on the same emulator, and the number
                                says whether the kernel knows how to wait. Parity with Linux here is
                                the claim -- and until REQ-CON-006 it was not true: Aletheia's
                                console loop spun, and an idle machine cost a whole core.

  typed echo round-trip         The same fair shape, under LOAD: N 'echo' round-trips typed into
                                each guest's shell, wall-clocked end-to-end. It exercises the whole
                                interactive path -- tty/input ring, line editor, dispatcher,
                                output formatting, serial transmission -- and doubles as a small
                                stress test: a dropped keystroke hangs the leg and FAILS it, never
                                passes quietly. Stated asymmetry: Aletheia's dispatcher runs in
                                KERNEL space while busybox sh runs in USER space over syscalls;
                                that is a design difference between the systems, not a controlled
                                variable, and this column prices it in rather than hiding it.

  privileged lines of code      The column that does not depend on an emulator, a host, or a
                                workload. It is how much code must be correct for the machine to be
                                correct, and it is the one number where Aletheia's design -- rather
                                than its youth -- is doing the work.

WHAT ALETHEIA DOES NOT WIN, stated because a benchmark that omits it is advertising:
  * throughput of anything real. There is no scheduler tuning, no page-cache, no SMP work stealing
    under load, and the filesystem is one flat namespace on one block device.
  * hardware support. Linux boots on the machine you own; Aletheia boots on three emulated boards.
  * everything in docs/MATURITY.md, which grades every subsystem and says plainly that nothing here
    is production-ready.
NOTE

[ "$fail" -eq 0 ] && echo "comparative-bench: PASS" || echo "comparative-bench: FAIL"
exit "$fail"
