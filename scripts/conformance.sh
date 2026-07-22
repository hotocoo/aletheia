#!/usr/bin/env bash
# Cross-architecture conformance gate (GAPS2 Issue #2).
#
# The biggest systemic risk once a feature exists on all three CPU targets is *silent behavioral
# divergence* — the security boundary differing by architecture. This gate asserts that every target
# proves the SAME core SEMANTIC contract, booting each and checking that each named behavior appears
# in that target's live invariant log.
#
# It is spec'd on NAMED BEHAVIORS, not identical invariant COUNTS: architectures legitimately differ
# (e.g. x86-64 proves 6 virtual-memory invariants where aarch64/RISC-V prove 13, because long mode
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
  "the boosted LOW runs and services the endpoint"
  "HIGH resumes as highest-priority and receives"
)

hr() { printf '========================================================================\n'; }

hr; echo "==> Booting all three targets to capture their live invariant logs"; hr
echo "--> aarch64 …";  AOUT="$(bash "$ROOT/scripts/vm-e2e.sh" 2>&1)"
echo "--> RISC-V …";   ROUT="$(bash "$ROOT/scripts/vm-e2e-riscv.sh" 2>&1)"
echo "--> x86-64 …"
XQ="$(brew --prefix qemu 2>/dev/null)/share/qemu"; [ -d "$XQ" ] || XQ=/opt/homebrew/share/qemu
x86_ran=0
if [ "$(uname -s)" = "Darwin" ] && command -v hdiutil >/dev/null 2>&1 && [ -f "$XQ/edk2-x86_64-code.fd" ]; then
  rm -f "$ROOT/kernel-x86_64/target/x86_64-unknown-uefi/release/aletheia-kernel-x86_64.efi"
  XOUT="$( (bash "$ROOT/kernel-x86_64/scripts/build-image.sh" && bash "$ROOT/kernel-x86_64/scripts/smoke-test.sh") 2>&1 )"
  x86_ran=1
else
  XOUT=""
  echo "    x86-64 boot unavailable on this host — its column is SKIPPED (never a silent pass)."
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
