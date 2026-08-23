#!/usr/bin/env bash
# Machine-checkable CI-coverage gate (GAPS4 ALET-P0-003).
#
# check-traceability.sh proves that a requirement claiming `delivered` points at evidence that
# EXISTS. It cannot prove that the evidence RUNS. ALET-P0-001 was exactly that hole: x86-64 was a
# first-class architecture with a boot script on disk and a requirement row naming it, while no CI
# job ever executed it — the claim was true on paper and unenforced in practice.
#
# This gate closes the loop with four mechanical checks:
#
#   [1] Architecture boot-gate coverage — every BOOTABLE kernel crate discovered on the filesystem
#       (a directory with Cargo.toml + src/main.rs) must have a boot-gate script that CI executes.
#       Discovery is from the tree, not a hand-written list, so adding a fourth CPU target without
#       a boot gate FAILS the build instead of quietly shipping an unqualified architecture.
#
#   [2] Every gate script in the tree is either executed by CI or explicitly exempt. This check used
#       to be a PARITY check between .github/workflows/ci.yml and a .gitlab-ci.yml, because the repo
#       once pushed to a self-hosted GitLab as well. **Aletheia is published to GitHub only**, the
#       GitLab pipeline was removed, and a parity check with one side deleted is a check that always
#       passes — which is worse than no check, because it reads like one that is working. What
#       replaces it is the question parity was a proxy for: is there a gate script sitting in
#       scripts/ that nothing runs?
#
#   [3] Claimed-gate enforcement — every path in the `VM Gate` column of docs/TRACEABILITY.md must
#       actually be executed by CI: either directly, or (for an aggregate runner like e2e-all.sh)
#       by having every script it invokes executed by CI.
#
#   [4] STATUS cross-check — every script CI executes must be named in STATUS.md, so the status
#       document cannot describe a qualification story that omits a gate that really runs.
#
# No new toolchain dependency: pure bash + coreutils, so it runs unchanged in the `rust:latest` CI
# image alongside the hosted-acceptance, VM-boot and traceability gates. Exit 0 = PASS.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GH="${GH_CI:-$ROOT/.github/workflows/ci.yml}"

MATRIX="${TRACEABILITY_MATRIX:-$ROOT/docs/TRACEABILITY.md}"
STATUS="${STATUS_DOC:-$ROOT/STATUS.md}"

for f in "$GH" "$MATRIX" "$STATUS"; do
  [ -f "$f" ] || { echo "FAIL: required file not found: $f"; exit 2; }
done

fail=0

# Index of every shell script in the tree as "basename<TAB>repo-relative-path". Resolution goes
# through this index rather than through the literal text of a path, because invocations are written
# with shell variables ("$ROOT/scripts/vm-e2e.sh", "$X86/scripts/smoke-test.sh") that no regex can
# turn back into a repo-relative path. Basenames are unique across the tree; the check below fails
# loudly if that ever stops being true, rather than silently resolving to the wrong script.
INDEX="$(find "$ROOT" -name '*.sh' -not -path '*/target/*' -not -path '*/.git/*' \
  | sed -e "s#^$ROOT/##" | awk -F/ '{ print $NF "\t" $0 }' | sort -u)"
dupes="$(printf '%s\n' "$INDEX" | cut -f1 | uniq -d)"
if [ -n "$dupes" ]; then
  echo "FAIL: duplicate script basenames make CI-reference resolution ambiguous:"
  printf '  %s\n' $dupes
  exit 2
fi

# Every script in the tree EXECUTED by a file, as repo-relative paths, deduplicated and sorted.
# Comments are stripped first: a script named in prose ("see e2e-all.sh") must not count as CI
# coverage — that would let a stale comment stand in for a job that actually runs. Names matching
# no script in the index (third-party examples) are dropped.
scripts_in() {
  local names
  names="$(sed -e 's/#.*//' "$1" 2>/dev/null | grep -oE '[A-Za-z0-9_.-]+\.sh' | sort -u)"
  [ -z "$names" ] && return 0
  awk -F'\t' 'NR == FNR { want[$0] = 1; next } ($1 in want) { print $2 }' \
    <(printf '%s\n' "$names") <(printf '%s\n' "$INDEX") | sort -u
}

# --- [1]+[2] the set of scripts CI actually executes -----------------------------------------
gh_set="$(scripts_in "$GH")"
ci_set="$gh_set"

# Scripts that exist to be run BY something else, or by a person, rather than by CI directly. Each
# one needs a reason, because "it is exempt" with no reason is how a gate stops being run.
#
#   e2e-all.sh, build-all.sh   aggregate runners; CI executes the scripts they invoke
#   run-interactive.sh         an operator sits at this one and types; there is nothing to assert
#   vbox-install.sh            installs a hypervisor onto an operator's machine
#   serial-console.ps1         a Windows helper for the VirtualBox walkthrough
#   linux_pipe_bench.sh        a host-side baseline for a discussion, superseded as a GATE by
#                              comparative-bench.sh, which CI does run
#   sbom.py                    invoked by quality-gate.sh
#   build-example-component.sh regenerates the example component's .wasm, which is COMMITTED — CI
#                              consumes the fixture (aletheia/tests/sdk_component.rs) rather than
#                              rebuilding it, so that a wasm toolchain is not a condition of running
#                              the test suite
#   lib-markers.sh             a SOURCED library of marker helpers — it executes inside each VM
#                              gate that CI runs (vm-e2e.sh, vm-e2e-riscv.sh, smoke-test.sh), the
#                              same relationship sbom.py has to quality-gate.sh
EXEMPT="e2e-all.sh build-all.sh run-interactive.sh vbox-install.sh serial-console.ps1 linux_pipe_bench.sh sbom.py build-example-component.sh lib-markers.sh"

echo "== [2] every gate script in scripts/ is executed by CI or explicitly exempt"
unrun=0
for path in "$ROOT"/scripts/*.sh; do
  base="$(basename "$path")"
  case " $EXEMPT " in *" $base "*) continue ;; esac
  if ! printf '%s\n' "$ci_set" | grep -qx "scripts/$base"; then
    echo "  FAIL: scripts/$base exists but no CI job executes it, and it is not exempt"
    unrun=1; fail=1
  fi
done
[ "$unrun" -eq 0 ] && echo "  PASS: every non-exempt gate script in scripts/ is executed by CI"

# Existence + executability of everything CI claims to run: a CI job invoking a missing or
# non-executable script fails only at push time, on the runner, after the fact.
#
# Executability is read from the GIT INDEX, not from the filesystem. On a Windows clone reached
# through WSL, `/mnt/c` is drvfs and reports EVERY file as mode 777, so `[ -x ]` is unconditionally
# true and this check could never fail on the host where the mode is most likely to be wrong. That
# is exactly what happened: `scripts/vm-e2e-vbox.sh` was committed 100644, every local run passed,
# and two CI jobs died with "Permission denied" — the same class of host-dependence as ALET-P2-035
# (CRLF). What CI executes is what git recorded, so that is what gets checked; the filesystem bit is
# the fallback only where git cannot answer (an exported tarball).
mode_of() {
  git -C "$ROOT" ls-files -s -- "$1" 2>/dev/null | awk '{print $1; exit}'
}
while IFS= read -r s; do
  [ -z "$s" ] && continue
  if [ ! -f "$ROOT/$s" ]; then
    echo "  FAIL: CI executes $s but it does not exist in the tree"; fail=1
    continue
  fi
  m="$(mode_of "$s")"
  if [ -n "$m" ]; then
    [ "$m" = "100755" ] || {
      echo "  FAIL: CI executes $s but git records it as $m, not 100755 (git update-index --chmod=+x $s)"
      fail=1
    }
  elif [ ! -x "$ROOT/$s" ]; then
    echo "  FAIL: CI executes $s but it is not executable (chmod +x)"; fail=1
  fi
done <<<"$ci_set"

# Transitive closure: a script CI runs, plus every script those scripts invoke (vm-e2e-x86.sh
# invokes build-image-linux.sh and smoke-test.sh, so those are CI-enforced too).
closure="$ci_set"
for _ in 1 2 3 4; do
  next="$closure"
  while IFS= read -r s; do
    { [ -z "$s" ] || [ ! -f "$ROOT/$s" ]; } && continue
    next="$next
$(scripts_in "$ROOT/$s")"
  done <<<"$closure"
  next="$(printf '%s\n' "$next" | sort -u | grep -v '^$')"
  [ "$next" = "$closure" ] && break
  closure="$next"
done

in_closure() { printf '%s\n' "$closure" | grep -qxF "$1"; }

# --- [1] every bootable kernel crate has a CI-executed boot gate ------------------------------
echo "== [1] Architecture boot-gate coverage (bootable kernel crates discovered from the tree)"
found_arch=0
for dir in "$ROOT"/*/; do
  crate="$(basename "$dir")"
  # A bootable target = its own crate with a binary entry point. kernel-core is a library
  # (no src/main.rs) and is covered by the host test suites, not by a boot gate.
  { [ -f "$dir/Cargo.toml" ] && [ -f "$dir/src/main.rs" ]; } || continue
  # Only kernel crates are architecture backends; aletheia/ is the hosted System Core.
  case "$crate" in kernel|kernel-*) ;; *) continue ;; esac
  found_arch=$((found_arch + 1))

  gate=""
  for cand in "$ROOT"/scripts/*.sh; do
    # A boot gate for crate K drives that crate's directory ("$ROOT/K"). The trailing quote keeps
    # "kernel" from matching kernel-riscv64 / kernel-x86_64.
    grep -qE "ROOT\}?/$crate\"" "$cand" || continue
    rel="scripts/$(basename "$cand")"
    if in_closure "$rel"; then gate="$rel"; break; fi
  done

  if [ -n "$gate" ]; then
    echo "  PASS: $crate boot-gated by $gate (executed by CI)"
  else
    echo "  FAIL: $crate is a bootable architecture with no CI-executed boot gate"
    fail=1
  fi
done
if [ "$found_arch" -eq 0 ]; then
  echo "  FAIL: no bootable kernel crates discovered — discovery rule broken?"; fail=1
fi

# --- [3] every gate claimed in the traceability matrix is executed by CI ----------------------
echo "== [3] Claimed VM gates are CI-enforced (docs/TRACEABILITY.md 'VM Gate' column)"
claimed="$(awk -F'|' '/^\| REQ-/ { gsub(/^[ \t]+|[ \t]+$/, "", $7); if ($7 != "-" && $7 != "") print $7 }' "$MATRIX" \
  | tr ';' '\n' | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' | sort -u | grep -v '^$')"
while IFS= read -r g; do
  [ -z "$g" ] && continue
  if in_closure "$g"; then
    echo "  PASS: $g"
  else
    # Deliberately strict: no "aggregate runner" exemption. A script that merely invokes CI-enforced
    # legs may still carry assertions of its own (conformance.sh compares the three logs against a
    # core semantic contract) that nothing would run. If a requirement names a gate, CI runs THAT
    # gate — or the requirement names the legs it actually relies on.
    echo "  FAIL: $g is claimed as a VM gate but no CI job executes it"
    fail=1
  fi
done <<<"$claimed"

# --- [4] STATUS.md names every gate CI runs ---------------------------------------------------
echo "== [4] STATUS cross-check (every CI-executed script is documented in STATUS.md)"
missing_status=0
while IFS= read -r s; do
  [ -z "$s" ] && continue
  if ! grep -qF "$(basename "$s")" "$STATUS"; then
    echo "  FAIL: CI executes $s but STATUS.md never mentions it"
    fail=1; missing_status=1
  fi
done <<<"$ci_set"
[ "$missing_status" -eq 0 ] && echo "  PASS: STATUS.md documents every CI-executed script"

echo "----------------------------------------"
if [ "$fail" -eq 0 ]; then
  echo "CI-PARITY: PASS (every architecture is boot-gated, every gate script runs, every claimed gate runs)"
  exit 0
else
  echo "CI-PARITY: FAIL (a claim is unenforced — see failures above)"
  exit 1
fi
