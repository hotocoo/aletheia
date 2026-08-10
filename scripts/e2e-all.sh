#!/usr/bin/env bash
# Unified end-to-end release gate for ALL Aletheia CPU targets (PRD §VV, ADR-013/019).
#
# One command, one pass/fail, three CPU targets on QEMU plus a SECOND HYPERVISOR rung:
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
#   4. AMD64/x86-64 on Oracle VirtualBox    — scripts/vm-e2e-vbox.sh (ADR-046)
#        the SAME image, booted by an independent implementation of the platform contract:
#        VirtualBox's own EFI, its own ACPI tables, SATA/AHCI, and no isa-debug-exit — so the
#        verdict is the serial log with marker parity against the QEMU gate. SKIPs (never
#        silent-passes) when VirtualBox is absent. Set REQUIRE_VBOX=1 to make a SKIP fatal.
#
# Exit 0 iff every leg that ran passed AND no required leg was skipped.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REQUIRE_X86="${REQUIRE_X86:-0}"
REQUIRE_VBOX="${REQUIRE_VBOX:-0}"

aarch64_res="not-run"
riscv_res="not-run"
x86_res="not-run"
vbox_res="not-run"

hr() { printf '========================================================================\n'; }

hr; echo "==> [1/4] aarch64 vm-e2e (full depth: spine + mm + vm + EL0/preemption)"; hr
if bash "$ROOT/scripts/vm-e2e.sh"; then aarch64_res="PASS"; else aarch64_res="FAIL"; fi

hr; echo "==> [2/4] RISC-V/RV64GC vm-e2e (S-mode + SBI + rdtime + spine + mm + Sv39 vm + U-mode)"; hr
if bash "$ROOT/scripts/vm-e2e-riscv.sh"; then riscv_res="PASS"; else riscv_res="FAIL"; fi

hr; echo "==> [3/4] AMD64/x86-64 disk-image boot smoke-test (UEFI + timer IRQ + spine)"; hr
# Portable leg: scripts/vm-e2e-x86.sh builds the .efi from HEAD, assembles a FAT ESP image with
# mtools (no hdiutil/loop devices), and boots it under QEMU+OVMF — so this leg now runs on Linux/CI
# at parity with the aarch64/RISC-V legs, not just macOS. It only SKIPs (never silent-passes) when
# the host lacks qemu-system-x86_64 + mtools + OVMF firmware.
X86="$ROOT/kernel-x86_64"
have_ovmf=0
for c in /opt/homebrew/share/qemu/edk2-x86_64-code.fd /usr/share/OVMF/OVMF_CODE_4M.fd \
         /usr/share/OVMF/OVMF_CODE.fd /usr/share/edk2/x64/OVMF_CODE.4m.fd; do
  [ -f "$c" ] && { have_ovmf=1; break; }
done
if command -v qemu-system-x86_64 >/dev/null 2>&1 && command -v mformat >/dev/null 2>&1 && [ "$have_ovmf" = "1" ]; then
  if bash "$ROOT/scripts/vm-e2e-x86.sh"; then x86_res="PASS"; else x86_res="FAIL"; fi
else
  x86_res="SKIP (needs qemu-system-x86_64 + mtools + OVMF firmware)"
  echo "    x86-64 image build/boot tooling unavailable on this host — leg skipped."
fi

hr; echo "==> [4/4] AMD64/x86-64 on a SECOND hypervisor: Oracle VirtualBox (ADR-046)"; hr
# The gate itself prints SKIP and exits 0 when VirtualBox is absent, so the leg is classified
# from its own summary line rather than from its exit code alone — an exit-0 SKIP must not be
# recorded here as a PASS.
vbox_out="$(bash "$ROOT/scripts/vm-e2e-vbox.sh" 2>&1)"; vbox_rc=$?
printf '%s
' "$vbox_out"
if printf '%s' "$vbox_out" | grep -q "VM-E2E-VBOX: SKIP"; then
  vbox_res="SKIP (VirtualBox absent, or a host that cannot virtualize x86-64 — the script says which)"
elif [ "$vbox_rc" -eq 0 ]; then vbox_res="PASS"; else vbox_res="FAIL"; fi

hr; echo "E2E SUMMARY"; hr
printf '  aarch64 (full)      : %s\n' "$aarch64_res"
printf '  riscv64 (full)      : %s\n' "$riscv_res"
printf '  x86-64  (image)     : %s\n' "$x86_res"
# The VirtualBox rung gates the exit code below, so it belongs in the summary too: a leg that can
# fail the run and does not appear here is a leg whose SKIP reads as "not attempted".
printf '  x86-64  (VirtualBox): %s\n' "$vbox_res"
hr

fail=0
[ "$aarch64_res" = "PASS" ] || fail=1
[ "$riscv_res" = "PASS" ]   || fail=1
case "$x86_res" in
  PASS) ;;
  SKIP*) [ "$REQUIRE_X86" = "1" ] && { echo "x86-64 skipped but REQUIRE_X86=1 -> fail"; fail=1; } ;;
  *) fail=1 ;;
esac
case "$vbox_res" in
  PASS) ;;
  SKIP*) [ "$REQUIRE_VBOX" = "1" ] && { echo "VirtualBox rung skipped but REQUIRE_VBOX=1 -> fail"; fail=1; } ;;
  *) fail=1 ;;
esac

if [ "$fail" -eq 0 ]; then echo "E2E-ALL: PASS"; exit 0; else echo "E2E-ALL: FAIL"; exit 1; fi
