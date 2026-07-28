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
#   [2] CI configuration parity — .github/workflows/ci.yml and .gitlab-ci.yml must execute the SAME
#       set of scripts. The repo pushes to both GitHub and the self-hosted GitLab origin; a gate
#       wired into only one of them is enforced for only half the pushes.
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
GL="${GL_CI:-$ROOT/.gitlab-ci.yml}"
MATRIX="${TRACEABILITY_MATRIX:-$ROOT/docs/TRACEABILITY.md}"
STATUS="${STATUS_DOC:-$ROOT/STATUS.md}"

for f in "$GH" "$GL" "$MATRIX" "$STATUS"; do
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
gl_set="$(scripts_in "$GL")"

echo "== [2] CI configuration parity (.github/workflows/ci.yml vs .gitlab-ci.yml)"
only_gh="$(comm -23 <(printf '%s\n' "$gh_set") <(printf '%s\n' "$gl_set"))"
only_gl="$(comm -13 <(printf '%s\n' "$gh_set") <(printf '%s\n' "$gl_set"))"
if [ -n "$only_gh" ]; then
  while IFS= read -r s; do [ -n "$s" ] && echo "  FAIL: $s runs in GitHub CI but not in GitLab CI"; done <<<"$only_gh"
  fail=1
fi
if [ -n "$only_gl" ]; then
  while IFS= read -r s; do [ -n "$s" ] && echo "  FAIL: $s runs in GitLab CI but not in GitHub CI"; done <<<"$only_gl"
  fail=1
fi
[ -z "$only_gh$only_gl" ] && echo "  PASS: both pipelines execute the same $(printf '%s\n' "$gh_set" | grep -c .) scripts"

# CI-executed set = union of both pipelines (parity is asserted separately above, so a wiring gap
# is reported once as a parity failure rather than cascading into every other check).
ci_set="$(printf '%s\n%s\n' "$gh_set" "$gl_set" | sort -u | grep -v '^$')"

# Existence + executability of everything CI claims to run: a CI job invoking a missing or
# non-executable script fails only at push time, on the runner, after the fact.
while IFS= read -r s; do
  [ -z "$s" ] && continue
  if [ ! -f "$ROOT/$s" ]; then
    echo "  FAIL: CI executes $s but it does not exist in the tree"; fail=1
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
  echo "CI-PARITY: PASS (every architecture is boot-gated, both pipelines agree, every claimed gate runs)"
  exit 0
else
  echo "CI-PARITY: FAIL (a claim is unenforced — see failures above)"
  exit 1
fi
