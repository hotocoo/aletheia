#!/usr/bin/env bash
# Attack-surface comparison against a real Linux kernel (REQ-SEC-001, ADR-083).
#
# WHY THIS EXISTS, AND WHAT IT REFUSES TO DO.
#
# `scripts/comparative-bench.sh` made "faster than Linux" measurable. Nothing in this repository
# made "safer than Linux" measurable, and until something does, every security sentence here is an
# adjective. Worse, security is the easiest thing in systems software to claim and the hardest to
# measure, so the temptation is to compare a design story against somebody else's CVE count.
#
# This script refuses that. It measures a small number of things that are COUNTABLE on both
# systems, from primary sources, and it SKIPs loudly rather than estimating. It does not produce a
# verdict, because attack surface is not a verdict.
#
# THE THREAT MODEL, NAMED. Everything below is about exactly one attacker:
#
#     Unprivileged code already executing on the machine, in user space, trying to obtain authority
#     it was not granted.
#
# Not physical access. Not a malicious hypervisor. Not supply chain — ADR-067 covers that
# separately. Not network-remote attackers, because Aletheia's network stack is not exposed the way
# a general-purpose OS's is and a comparison would be dishonest. One attacker, stated up front, so
# that a reader can tell what a number is evidence FOR.
#
# WHAT IS AND IS NOT BEING COMPARED. Linux 6.12 is a general-purpose kernel running the world's
# infrastructure, with namespaces, seccomp, SELinux/AppArmor, and thirty years of hardening against
# real attackers who were really trying. Aletheia is a microkernel with eleven syscalls that has
# never been attacked by anyone. A smaller surface is NOT the same as a safer system: surface is one
# input to risk, and the other inputs — maturity, review, exploit economics, defense in depth —
# favour Linux overwhelmingly and are not measured here at all.
#
# RUNNING IT:  ./scripts/security-surface.sh
# Needs network access to fetch Linux's syscall table from the kernel tree (a primary source, not a
# blog post). SKIPs that column when offline.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

LINUX_TAG="${LINUX_TAG:-v6.12}"
SYSCALL_TBL_URL="${SYSCALL_TBL_URL:-https://raw.githubusercontent.com/torvalds/linux/$LINUX_TAG/arch/x86/entry/syscalls/syscall_64.tbl}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

hr() { printf '========================================================================\n'; }
fail=0

hr; echo "==> attack surface: Aletheia vs Linux $LINUX_TAG (threat model: unprivileged local code"
echo "    already executing, trying to obtain authority it was not granted)"; hr

# --- 1. syscall surface -------------------------------------------------------------------------
# Aletheia's is counted from the ABI decode table itself, not from a doc that could drift.
AL_SYSCALLS="$(grep -cE '^pub const SYS_[A-Z_]+: u64 = [0-9]+;$' kernel-core/src/syscall.rs)"
# Of those, how many consult a named object capability before the effect happens.
AL_CAP_GATED="$(sed -n '/pub const fn capability/,/^    }$/p' kernel-core/src/syscall.rs \
  | grep -cE '=> Some\("')"
# A capability arm may name several syscalls (`Self::Send | Self::Recv`), so count the variants.
AL_CAP_VARIANTS="$(sed -n '/pub const fn capability/,/^    }$/p' kernel-core/src/syscall.rs \
  | grep -E '=> Some\("' | grep -oE 'Self::[A-Za-z]+' | wc -l | tr -d ' ')"

echo "--> Aletheia syscall surface (counted from kernel-core/src/syscall.rs)"
echo "    syscalls in the ABI:                 $AL_SYSCALLS"
echo "    of which gated by a named capability: $AL_CAP_VARIANTS (in $AL_CAP_GATED policy arms)"
echo "    remainder carry no object authority:  $((AL_SYSCALLS - AL_CAP_VARIANTS)) (yield, exit, register check)"

LX_SYSCALLS="SKIP"
echo "--> Linux syscall surface (counted from the kernel tree's own syscall_64.tbl)"
if curl -sL --max-time 60 -o "$WORK/syscall_64.tbl" "$SYSCALL_TBL_URL" && [ -s "$WORK/syscall_64.tbl" ]; then
  LX_SYSCALLS="$(awk '$1 ~ /^[0-9]+$/ && ($2 == "common" || $2 == "64") {n++} END {print n+0}' "$WORK/syscall_64.tbl")"
  echo "    x86-64 syscalls (common + 64):        $LX_SYSCALLS"
else
  echo "    SKIPPED — could not fetch $SYSCALL_TBL_URL (never a silent pass)"
fi

# --- 2. privileged code size --------------------------------------------------------------------
AL_PRIV_LOC="$(find kernel-core/src kernel/src kernel-riscv64/src kernel-x86_64/src -name '*.rs' \
  -exec cat {} + 2>/dev/null | grep -vcE '^\s*(//|$)')"
echo "--> privileged code (the code that runs with the machine's full authority)"
echo "    Aletheia, counted, non-comment Rust:  $AL_PRIV_LOC"
echo "    Linux 6.12, cited, C:                 ~40,000,000 (not counted here; see the honesty note)"

# --- 3. memory-unsafe surface -------------------------------------------------------------------
AL_UNSAFE="$(grep -rhoE '\bunsafe\b' kernel-core/src kernel/src kernel-riscv64/src kernel-x86_64/src \
  --include='*.rs' 2>/dev/null | wc -l | tr -d ' ')"
echo "--> memory-unsafe surface inside privileged code"
printf '    Aletheia: %s `unsafe` occurrences in %s lines — %.2f%% of privileged lines\n' \
  "$AL_UNSAFE" "$AL_PRIV_LOC" \
  "$(awk -v u="$AL_UNSAFE" -v l="$AL_PRIV_LOC" 'BEGIN{print (l?100*u/l:0)}')"
echo "    Linux:    n/a — C has no such marker, so 100% of its privileged code is memory-unsafe by"
echo "              language and none of it is delimited. This is a LANGUAGE difference, not a"
echo "              measurement of either kernel's defect density."

# --- 4. ambient authority -----------------------------------------------------------------------
# This is the design claim, stated as one and checked against the tree rather than asserted.
echo "--> ambient authority on the syscall path"
if grep -q 'pub const fn capability' kernel-core/src/syscall.rs; then
  echo "    Aletheia: every effect-bearing syscall names a capability action in ONE table"
  echo "              (Syscall::capability), and the boundary consults it before the effect."
  echo "              Proved on every boot by the ring-3/EL0/U-mode suites, not by this script."
else
  echo "  FAIL: the capability table this claim depends on is gone"; fail=1
fi
echo "    Linux:    a process acts with the ambient authority of its uid plus its capability set;"
echo "              confinement is OPT-IN and external (seccomp, SELinux/AppArmor, namespaces)."
echo "              That is a different model, deliberately, and it carries workloads this one"
echo "              cannot run."

# --- the table ----------------------------------------------------------------------------------
hr; echo "SUMMARY — attack surface only, under the threat model named at the top"; hr
printf '%-40s | %-20s | %-20s\n' "" "Aletheia" "Linux $LINUX_TAG"
printf '%-40s-+-%-20s-+-%-20s\n' "$(printf '%.0s-' {1..40})" "$(printf '%.0s-' {1..20})" "$(printf '%.0s-' {1..20})"
printf '%-40s | %-20s | %-20s\n' "syscalls exposed to user space" "$AL_SYSCALLS" "$LX_SYSCALLS"
printf '%-40s | %-20s | %-20s\n' "of which capability-gated" "$AL_CAP_VARIANTS" "0 (model differs)"
printf '%-40s | %-20s | %-20s\n' "privileged lines of code" "$AL_PRIV_LOC" "~40M (cited)"
printf '%-40s | %-20s | %-20s\n' "privileged code in a safe language" "yes, except $AL_UNSAFE unsafe" "no"

if [ "$LX_SYSCALLS" != "SKIP" ] && [ "$AL_SYSCALLS" -gt 0 ]; then
  printf '\n    syscall surface ratio: Linux exposes %.1fx as many entry points.\n' \
    "$(awk -v a="$AL_SYSCALLS" -v l="$LX_SYSCALLS" 'BEGIN{print l/a}')"
fi

cat <<'NOTE'

HOW TO READ THIS — and what it is NOT evidence for.

  A smaller surface is not a safer system. Surface is ONE input to risk. The others — maturity,
  adversarial review, exploit economics, defense in depth, and the simple fact that Linux has been
  attacked by professionals for thirty years while Aletheia has never been attacked by anyone —
  are not measured here, and every one of them favours Linux.

  The syscall ratio is real and it is also unfair. Linux's 300+ entry points exist because it runs
  containers, graphics stacks, io_uring, BPF, and thirty years of ABI promises. Aletheia has
  eleven because it does almost nothing yet. Removing features is not a security technique; the
  claim worth making is narrower, and it is this: on the syscall path Aletheia has, authority is
  named in one table and checked before the effect, rather than being ambient and confined later
  by an opt-in mechanism. That is a DESIGN difference and it is what the boot-gated ring-3/EL0/
  U-mode suites prove.

  "capability-gated: 0" for Linux is not a criticism and not a bug. Linux's model is ambient
  authority bounded by uid and capability sets, with confinement added on top. Counting it as zero
  in this column means "this kernel does not gate syscalls on per-object capabilities", which is
  true and intentional, not a finding.

  NOT MEASURED HERE AT ALL: exploitability, defect density, CVE history, side channels,
  speculative execution, firmware, supply chain (ADR-067), or any other operating system. Windows,
  macOS, the BSDs and every RTOS are absent. No claim is made about them.

  NOT A VERDICT. This script prints a surface comparison. It does not say which system is safer,
  because that question is not answered by counting, and docs/MATURITY.md says plainly that nothing
  here is production-ready.
NOTE

hr
if [ "$fail" -eq 0 ]; then
  echo "security-surface: PASS (surface measured; no verdict claimed)"
else
  echo "security-surface: FAIL"
fi
exit "$fail"
