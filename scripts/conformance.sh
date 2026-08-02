#!/usr/bin/env bash
# Cross-architecture conformance gate (GAPS2 Issue #2).
#
# The biggest systemic risk once a feature exists on all three CPU targets is *silent behavioral
# divergence* — the security boundary differing by architecture. This gate asserts that every target
# proves the SAME core SEMANTIC contract, booting each and checking that each named behavior appears
# in that target's live invariant log.
#
# It is spec'd on NAMED BEHAVIORS, not identical invariant COUNTS: architectures legitimately differ
# (e.g. x86-64 proves 13 virtual-memory invariants where aarch64/RISC-V prove 21, because long mode
# cannot do the MMU-off→on flip — an honest arch difference, not a regression). Such per-arch invariants
# are EXTENSIONS, reported informationally, never conformance failures. Only the core contract below is
# required of all three.
#
# Exit 0 iff every core behavior is proved by every target.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# The CORE CONTRACT: arch-neutral substrings (no el0/u-mode/ring3 privilege term, no TTBR0/satp/PML4
# address-space term, no svc/ecall/int-0x80 trap term) that each target's boot log MUST contain. These
# are the capability-secure user-mode + IPC semantics every Aletheia backend must reproduce identically.
CONTRACT=(
  "capability-secure IPC — message delivered kernel-mediated across distinct address spaces"
  "shared-memory grant is capability-gated"
  "grant-table maps one frame into two distinct"
  "a successful grant revoke gates the unmap"
  "recv on an empty endpoint BLOCKS the receiver"
  "a send WAKES the blocked receiver"
  "the woken receiver RESUMES past its"
  "scheduler dispatches boosted LOW over Ready MEDIUM"
  # Mapping-API admission check (ALET-P1-001, REQ-MM-001). Worded identically on every target on
  # purpose: these are the refusals that must NOT differ by architecture, since a target that
  # accepts an address the others refuse is a security boundary that varies by CPU.
  "mapping an unaligned VA is refused"
  "mapping an unaligned PA is refused"
  "mapping a PA outside the frame-allocator window is refused"
  "mapping the null page is refused"
  # Frame-ownership model (ALET-P1-003, REQ-MM-002, ADR-030). The physical-memory twin of the
  # rules above: a double free hands ONE page to two owners, and freeing a frame you do not hold
  # takes a live page from whoever does. Every target must refuse both, in the same words — an
  # architecture where a double free is accepted is a different memory-safety boundary.
  "double free is refused (would hand one page to two owners)"
  "freeing another owner's frame is refused (fail-closed)"
  "freeing a never-allocated frame is refused"
  "a freed frame has no owner"
  "ownership table and free list agree on the free count"
  # Page-table reclamation (ALET-P1-002, REQ-MM-003, ADR-031). The level COUNT differs honestly by
  # architecture (3-level aarch64/Sv39 vs 4-level x86-64), so the contract names the behavior, not
  # the number: a sibling mapping must protect the tables, an emptied chain must come back to the
  # allocator, the root must survive, and the address space must still work afterwards.
  "unmapping one of two pages in a leaf table reclaims NO table (sibling still mapped)"
  "no table frame was returned while the leaf table was still in use"
  "the reclaimed table frames came back to the allocator"
  "neither VA resolves after reclamation"
  "the address space rebuilds the reclaimed chain (root intact, frames reusable)"
  # Address-space destruction (ALET-P1-004, REQ-MM-004, ADR-032). A dying space must give back
  # everything it owned and nothing else, and no target may allow the running kernel to destroy the
  # space it is executing in.
  "destroying the ACTIVE address space is refused (the kernel is running in it)"
  "teardown freed exactly the pages the space owned"
  "every table in the tree was one this space owned"
  "destroying the space returned every frame it held, including its root"
  "the surviving address space is intact after the teardown"
  "the boosted LOW runs and services the endpoint"
  "HIGH resumes as highest-priority and receives"
)

hr() { printf '========================================================================\n'; }

hr; echo "==> Booting all three targets to capture their live invariant logs"; hr
echo "--> aarch64 …";  AOUT="$(bash "$ROOT/scripts/vm-e2e.sh" 2>&1)"
echo "--> RISC-V …";   ROUT="$(bash "$ROOT/scripts/vm-e2e-riscv.sh" 2>&1)"
echo "--> x86-64 …"
# Drive the portable leg (scripts/vm-e2e-x86.sh: mtools ESP, no hdiutil/root) so the x86-64 column
# is really compared on Linux/CI too, not only on a macOS workstation. It builds the .efi from HEAD
# and boots under QEMU+OVMF. The leg SKIPs — never silently passes — when the host lacks the tools.
x86_ran=0
have_ovmf=0
for c in /opt/homebrew/share/qemu/edk2-x86_64-code.fd /usr/share/OVMF/OVMF_CODE_4M.fd \
         /usr/share/OVMF/OVMF_CODE.fd /usr/share/edk2/x64/OVMF_CODE.4m.fd; do
  [ -f "$c" ] && { have_ovmf=1; break; }
done
if command -v qemu-system-x86_64 >/dev/null 2>&1 && command -v mformat >/dev/null 2>&1 && [ "$have_ovmf" = "1" ]; then
  XOUT="$(bash "$ROOT/scripts/vm-e2e-x86.sh" 2>&1)"
  x86_ran=1
else
  XOUT=""
  echo "    x86-64 boot unavailable on this host (needs qemu-system-x86_64 + mtools + OVMF) — its column is SKIPPED (never a silent pass)."
fi

fail=0
report_target() {
  # $1 = label, $2 = the captured log
  local label="$1" log="$2" b missing=0
  for b in "${CONTRACT[@]}"; do
    if ! grep -qF "$b" <<<"$log"; then
      echo "  FAIL [$label] missing core behavior: $b"
      missing=1; fail=1
    fi
  done
  if [ "$missing" -eq 0 ]; then
    # Count arch-specific invariants proved (informational — the per-arch EXTENSIONS).
    local n
    n="$(grep -cE '\[pass +[0-9]+\]' <<<"$log")"
    echo "  PASS [$label] proves all ${#CONTRACT[@]} core behaviors (${n} total invariants incl. arch extensions)"
  fi
}

hr; echo "CONFORMANCE — core contract (${#CONTRACT[@]} named behaviors) across targets"; hr
report_target "aarch64" "$AOUT"
report_target "riscv64" "$ROUT"
if [ "$x86_ran" -eq 1 ]; then
  report_target "x86-64 " "$XOUT"
else
  echo "  SKIP [x86-64 ] not booted on this host"
fi

hr
if [ "$fail" -eq 0 ]; then
  echo "CONFORMANCE: PASS — every booted target proves the same core semantic contract"
  exit 0
else
  echo "CONFORMANCE: FAIL — a target diverged (missing a core behavior another target proves)"
  exit 1
fi
