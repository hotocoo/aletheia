#!/usr/bin/env bash
# Machine-checkable architecture traceability gate (gap-register Issue 12).
#
# Parses docs/TRACEABILITY.md (the requirement matrix) and enforces that every requirement marked
# `delivered` or `partial` maps to Implementation AND Test evidence that ACTUALLY EXISTS in the tree
# (each ';'-separated path is checked), plus any named VM-gate script. A requirement claimed as
# delivered without evidence — or carrying an unknown status — FAILS the build. `deferred` rows are
# documented (ADR/plan) but carry no code, and are never counted as delivered.
#
# No new toolchain dependency: pure bash + coreutils, so it runs unchanged in the `rust:latest` CI
# image alongside the hosted-acceptance and VM-boot gates.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# Matrix path is overridable (TRACEABILITY_MATRIX) so the gate itself is testable against fixtures.
MATRIX="${TRACEABILITY_MATRIX:-$ROOT/docs/TRACEABILITY.md}"
[ -f "$MATRIX" ] || { echo "FAIL: matrix not found at $MATRIX"; exit 2; }

fail=0
total=0
delivered=0
partial=0
deferred=0

trim() { printf '%s' "$1" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//'; }

check_paths() {
  # $1 = ';'-separated field, $2 = req id, $3 = column label
  local field="$1" req="$2" col="$3" p
  local old_ifs="$IFS"
  IFS=';'
  # shellcheck disable=SC2086
  set -- $field
  IFS="$old_ifs"
  for p in "$@"; do
    p="$(trim "$p")"
    [ -z "$p" ] && continue
    if [ ! -e "$ROOT/$p" ]; then
      echo "FAIL [$req] $col path does not exist: $p"
      fail=1
    fi
  done
}

while IFS= read -r line; do
  case "$line" in
    "| REQ-"*) ;;
    *) continue ;;
  esac
  IFS='|' read -r _lead req _title _adr impl test gate status _rest <<< "$line"
  req="$(trim "$req")"
  impl="$(trim "$impl")"
  test="$(trim "$test")"
  gate="$(trim "$gate")"
  status="$(trim "$status")"
  total=$((total + 1))

  case "$status" in
    delivered | partial)
      if [ "$status" = delivered ]; then delivered=$((delivered + 1)); else partial=$((partial + 1)); fi
      if [ "$impl" = "-" ] || [ -z "$impl" ]; then
        echo "FAIL [$req] status=$status but no Implementation evidence"; fail=1
      else
        check_paths "$impl" "$req" "Implementation"
      fi
      if [ "$test" = "-" ] || [ -z "$test" ]; then
        echo "FAIL [$req] status=$status but no Test evidence"; fail=1
      else
        check_paths "$test" "$req" "Test"
      fi
      if [ "$gate" != "-" ] && [ -n "$gate" ]; then
        check_paths "$gate" "$req" "VM Gate"
      fi
      ;;
    deferred)
      deferred=$((deferred + 1))
      ;;
    *)
      echo "FAIL [$req] unknown Status '$status' (want delivered|partial|deferred)"
      fail=1
      ;;
  esac
done < "$MATRIX"

echo "----------------------------------------"
echo "traceability: $total requirements — $delivered delivered, $partial partial, $deferred deferred"
if [ "$total" -eq 0 ]; then
  echo "TRACEABILITY: FAIL (no REQ- rows parsed — matrix format changed?)"
  exit 1
fi
if [ "$fail" -eq 0 ]; then
  echo "TRACEABILITY: PASS (every delivered/partial requirement maps to existing evidence)"
  exit 0
else
  echo "TRACEABILITY: FAIL (a requirement claims delivery without evidence, or has an unknown status)"
  exit 1
fi
