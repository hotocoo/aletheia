#!/usr/bin/env bash
# System 1 under load and under hostile input (ADR-188).
#
#   1. a seeded hostile stream (STORM_REQUESTS, STORM_SEED) through the dual path with the control
#      arm behind it: no unsafe line may come back, the sidecar must still answer after
#   2. STORM_PARALLEL storms at once against the same sidecar: every one completes, none errors
#   3. wire abuse straight at the sidecar: an oversized body, malformed JSON, an unknown question
#      type and a choice with no options are refused with 4xx, and it still serves afterwards
#
# SKIPs, by name, when no System-1 model is serving (`aletheiad model status`).
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ALETHEIAD="$ROOT/aletheia/target/release/aletheiad"
N="${STORM_REQUESTS:-1000}"; SEED="${STORM_SEED:-188}"; PAR="${STORM_PARALLEL:-4}"
fail=0
( cd "$ROOT/aletheia" && cargo build --release -q ) || { echo "FAIL: build"; exit 3; }
status="$("$ALETHEIAD" model status 2>/dev/null)"
if ! printf '%s\n' "$status" | grep '^system1: .* — serving' >/dev/null; then
  echo "SKIP: no System-1 model is serving — see \`aletheiad model status\`"; exit 0
fi
endpoint="$(printf '%s\n' "$status" | sed -n 's/^system1: .* at \(http[^ ]*\) — serving.*/\1/p')"

echo "==> [1] sequential storm: $N requests, seed $SEED"
out="$("$ALETHEIAD" console system1-storm --requests "$N" --seed "$SEED")"; rc=$?
printf '%s\n' "$out" | sed 's/^/  /'
[ "$rc" -eq 0 ] || { echo "  FAIL: storm rc=$rc"; fail=1; }
printf '%s\n' "$out" | grep -q '(hazards 0)' || { echo "  FAIL: an unsafe line came back"; fail=1; }
printf '%s\n' "$out" | grep -q '0 error;' || { echo "  FAIL: System 1 errored under the storm"; fail=1; }

echo "==> [2] $PAR concurrent storms of $((N / 4)) requests"
tmp="$(mktemp -d)"
for i in $(seq 1 "$PAR"); do
  "$ALETHEIAD" console system1-storm --requests "$((N / 4))" --seed "$((SEED + i))" > "$tmp/$i.log" 2>&1 &
done
wait
for i in $(seq 1 "$PAR"); do
  line="$(grep '^storm:' "$tmp/$i.log")"
  echo "  #$i ${line#storm: }"
  grep -q '(hazards 0)' "$tmp/$i.log" || { echo "  FAIL: storm #$i unsafe or incomplete"; fail=1; }
  grep -q '0 error;' "$tmp/$i.log" || { echo "  FAIL: storm #$i saw System-1 errors (timeouts under contention?)"; fail=1; }
done
rm -rf "$tmp"

echo "==> [3] wire abuse at $endpoint"
code() { curl -s -o /dev/null -w '%{http_code}' -m 10 "$@"; }
big="$(mktemp)"; head -c 300000 /dev/zero | tr '\0' 'a' > "$big"
for probe in \
  "413|oversized body|-X POST --data-binary @$big $endpoint/v1/decide" \
  "400|malformed JSON|-X POST -d {not-json $endpoint/v1/decide" \
  "400|unknown question type|-X POST -d {\"state\":\"x\",\"questions\":[{\"type\":\"essay\"}]} $endpoint/v1/decide" \
  "400|a choice with no options|-X POST -d {\"state\":\"x\",\"questions\":[{\"type\":\"choice\",\"options\":[]}]} $endpoint/v1/decide" \
  "400|a choice with one option|-X POST -d {\"state\":\"x\",\"questions\":[{\"type\":\"choice\",\"options\":[{\"label\":\"a\"}]}]} $endpoint/v1/decide" \
  "404|an unknown path|$endpoint/v1/shell"; do
  want="${probe%%|*}"; rest="${probe#*|}"; what="${rest%%|*}"; argv="${rest#*|}"
  # shellcheck disable=SC2086
  got="$(code $argv)"
  if [ "$got" = "$want" ]; then echo "  PASS: $what -> $got"; else echo "  FAIL: $what -> $got (want $want)"; fail=1; fi
done
rm -f "$big"
[ "$(code "$endpoint/v1/models")" = "200" ] && echo "  PASS: still serving after the abuse" \
  || { echo "  FAIL: the sidecar stopped serving"; fail=1; }

[ "$fail" -eq 0 ] && echo "SYSTEM1-STORM-E2E: PASS" || echo "SYSTEM1-STORM-E2E: FAIL"
exit "$fail"
