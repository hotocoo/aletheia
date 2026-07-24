#!/usr/bin/env bash
# Unified end-to-end release gate for ALL Aletheia CPU targets (PRD §VV, ADR-013/019).
#
# One command, one pass/fail, three targets:
#   1. aarch64 (bootstrap, full depth) — scripts/vm-e2e.sh
#        spine(11) + memory-management(7) + virtual-memory(13) + EL0-boundary(10:
#        cap-gated syscall, per-process isolation, round-robin scheduling, timer preemption).
#   2. RISC-V/RV64GC (second first-class)  — scripts/vm-e2e-riscv.sh
#        S-mode boot + SBI + rdtime + spine(11).
#   3. AMD64/x86-64 (first-class)          — kernel-x86_64 build-image + smoke-test
#        UEFI boot + arch init + PIT timer IRQ + memory(7) + vm(6) + spine(11) + SMP(13, MADT +
#        INIT-SIPI-SIPI at -smp 4) + ring-3(22), booted from the real disk image.
#
# aarch64 and RISC-V are pure QEMU and always run. The x86-64 leg builds a bootable GPT/ESP
# disk image, which needs a macOS host with hdiutil/diskutil + OVMF firmware; when that host
# tooling is absent the x86-64 leg is reported as SKIP (never a silent pass) so the summary
# never overstates coverage. Set REQUIRE_X86=1 to turn an x86-64 SKIP into a hard failure (CI).
#
# Exit 0 iff every leg that ran passed AND no required leg was skipped.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REQUIRE_X86="${REQUIRE_X86:-0}"

aarch64_res="not-run"
riscv_res="not-run"
x86_res="not-run"

hr() { printf '========================================================================\n'; }

hr; echo "==> [1/3] aarch64 vm-e2e (full depth: spine + mm + vm + EL0/preemption)"; hr
if bash "$ROOT/scripts/vm-e2e.sh"; then aarch64_res="PASS"; else aarch64_res="FAIL"; fi

hr; echo "==> [2/3] RISC-V/RV64GC vm-e2e (S-mode + SBI + rdtime + spine + mm + Sv39 vm + U-mode)"; hr
if bash "$ROOT/scripts/vm-e2e-riscv.sh"; then riscv_res="PASS"; else riscv_res="FAIL"; fi

hr; echo "==> [3/3] AMD64/x86-64 disk-image boot smoke-test (UEFI + timer IRQ + spine)"; hr
X86="$ROOT/kernel-x86_64"
QSHARE="$(brew --prefix qemu 2>/dev/null)/share/qemu"; [ -d "$QSHARE" ] || QSHARE=/opt/homebrew/share/qemu
if [ "$(uname -s)" = "Darwin" ] && command -v hdiutil >/dev/null 2>&1 && [ -f "$QSHARE/edk2-x86_64-code.fd" ]; then
  # Build from current source: drop the cached .efi so the image reflects HEAD, not a stale artifact.
  rm -f "$X86/target/x86_64-unknown-uefi/release/aletheia-kernel-x86_64.efi"
  if bash "$X86/scripts/build-image.sh" && bash "$X86/scripts/smoke-test.sh"; then
    x86_res="PASS"
  else
    x86_res="FAIL"
  fi
else
  x86_res="SKIP (needs macOS host + hdiutil + OVMF firmware)"
  echo "    x86-64 image build unavailable on this host — leg skipped."
fi

hr; echo "E2E SUMMARY"; hr
printf '  aarch64 (full)      : %s\n' "$aarch64_res"
printf '  riscv64 (full)      : %s\n' "$riscv_res"
printf '  x86-64  (image)     : %s\n' "$x86_res"
hr

fail=0
[ "$aarch64_res" = "PASS" ] || fail=1
[ "$riscv_res" = "PASS" ]   || fail=1
case "$x86_res" in
  PASS) ;;
  SKIP*) [ "$REQUIRE_X86" = "1" ] && { echo "x86-64 skipped but REQUIRE_X86=1 -> fail"; fail=1; } ;;
  *) fail=1 ;;
esac

if [ "$fail" -eq 0 ]; then echo "E2E-ALL: PASS"; exit 0; else echo "E2E-ALL: FAIL"; exit 1; fi
