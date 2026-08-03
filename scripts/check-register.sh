#!/usr/bin/env bash
# The register checks itself (GAPS4 ALET-P2-012, REQ-QUAL-003).
#
# `check-traceability.sh` proves every DELIVERED requirement names evidence that exists. Nothing proved the
# same of the gap register — and the register is where this project's claims about its own completeness
# live. Three ways it could drift, each checked here:
#
#   [1] a `resolved` row citing a file that no longer exists (a claim whose evidence was deleted or moved)
#   [2] a row citing a REQ- id that is not in the traceability matrix (a requirement nobody tracks)
#   [3] the ROLLUP arithmetic disagreeing with the rows (the anti-drift line drifting itself — which is the
#       one failure that would quietly invalidate every count this project reports)
#
# Exit 0 iff the register is internally consistent and its evidence exists.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REG="${REGISTER:-$ROOT/docs/gap/ARCHITECTURE-GAPS4-REGISTER.md}"
MATRIX="${TRACEABILITY_MATRIX:-$ROOT/docs/TRACEABILITY.md}"
[ -f "$REG" ] || { echo "FAIL: register not found at $REG"; exit 2; }
[ -f "$MATRIX" ] || { echo "FAIL: traceability matrix not found at $MATRIX"; exit 2; }

fail=0
resolved=0
open_rows=0
deferred=0
checked_paths=0

hr() { printf '%s\n' "----------------------------------------"; }

echo "== [1] every resolved row's cited files exist"
while IFS= read -r line; do
  case "$line" in "| ALET-"*) ;; *) continue ;; esac
  id="$(printf '%s' "$line" | awk -F'|' '{print $2}' | tr -d ' ')"
  disp="$(printf '%s' "$line" | awk -F'|' '{print $4}' | tr -d ' ')"
  case "$disp" in
    resolved) resolved=$((resolved + 1)) ;;
    open) open_rows=$((open_rows + 1)); continue ;;
    deferred) deferred=$((deferred + 1)); continue ;;
    *) echo "  FAIL [$id] unknown disposition '$disp' (want resolved|open|deferred)"; fail=1; continue ;;
  esac
  # Every backticked token that looks like a repo path must exist. Anything else in backticks (an API
  # name, a marker string) is ignored: the point is evidence, not vocabulary.
  for tok in $(printf '%s' "$line" | grep -oE '`[A-Za-z0-9_./-]+`' | tr -d '`'); do
    case "$tok" in
      */*.rs | */*.sh | */*.md | */*.toml | */*.ld | */*.yml)
        checked_paths=$((checked_paths + 1))
        if [ ! -e "$ROOT/$tok" ]; then
          echo "  FAIL [$id] resolved row cites a path that does not exist: $tok"
          fail=1
        fi
        ;;
    esac
  done
done < "$REG"
echo "  checked $checked_paths cited path(s) across $resolved resolved row(s)"

echo "== [2] every REQ- id the register cites is tracked in the traceability matrix"
for req in $(grep -oE 'REQ-[A-Z]+-[0-9]+' "$REG" | sort -u); do
  if ! grep -q "| $req |" "$MATRIX"; then
    echo "  FAIL: the register cites $req, which the traceability matrix does not track"
    fail=1
  fi
done

echo "== [3] the rollup arithmetic matches the rows"
# The rollup line states resolved/open/deferred; those must equal what the table actually contains.
claim_resolved="$(grep -oE '\*\*[0-9]+ resolved\*\*' "$REG" | head -1 | grep -oE '[0-9]+')"
claim_open="$(grep -oE '[0-9]+ open' "$REG" | head -1 | grep -oE '[0-9]+')"
claim_deferred="$(grep -oE '[0-9]+ deferred' "$REG" | head -1 | grep -oE '[0-9]+')"
for pair in "resolved:$claim_resolved:$resolved" "open:$claim_open:$open_rows" "deferred:$claim_deferred:$deferred"; do
  name="${pair%%:*}"; rest="${pair#*:}"; claimed="${rest%%:*}"; actual="${rest#*:}"
  if [ -z "$claimed" ]; then
    echo "  FAIL: the rollup does not state a $name count"
    fail=1
  elif [ "$claimed" != "$actual" ]; then
    echo "  FAIL: the rollup claims $claimed $name, the table has $actual"
    fail=1
  else
    echo "  PASS: $name = $actual (rollup agrees)"
  fi
done

hr
if [ "$fail" -eq 0 ]; then
  echo "REGISTER: PASS (evidence exists, requirements are tracked, the rollup matches the rows)"
  exit 0
else
  echo "REGISTER: FAIL (the register's own claims do not hold)"
  exit 1
fi
