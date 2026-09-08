#!/usr/bin/env bash
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${PROPERTY_ARTIFACT_DIR:-$ROOT/build/property-campaign}"
CASES="${ALETHEIA_PROPERTY_CASES:-64}"
SEED="${ALETHEIA_PROPERTY_SEED:-a1e702105eed}"
mkdir -p "$OUT"
rm -f "$OUT"/run.log "$OUT"/result.txt "$OUT"/seed.txt "$OUT"/failure.txt
case "$CASES" in ''|*[!0-9]*) echo "FAIL: ALETHEIA_PROPERTY_CASES must be an integer"; exit 2;; esac
[ "$CASES" -gt 0 ] || { echo "FAIL: ALETHEIA_PROPERTY_CASES must be greater than zero"; exit 2; }
SEED_HEX="${SEED#0x}"
cat > "$OUT/seed.txt" <<EOF
seed=0x${SEED_HEX}
cases=${CASES}
command=cargo test --manifest-path kernel-core/Cargo.toml --test property_campaign -- --nocapture
EOF
export ALETHEIA_PROPERTY_CASES="$CASES" ALETHEIA_PROPERTY_SEED="$SEED_HEX"
set +e
cargo test --manifest-path "$ROOT/kernel-core/Cargo.toml" --test property_campaign -- --nocapture 2>&1 | tee "$OUT/run.log"
status=${PIPESTATUS[0]}
set -e
if [ "$status" -ne 0 ]; then
  grep 'PROPERTY FAILURE ' "$OUT/run.log" > "$OUT/failure.txt" || printf '%s\n' 'No structured PROPERTY FAILURE line was emitted; see run.log.' > "$OUT/failure.txt"
  printf 'FAIL status=%s\n' "$status" > "$OUT/result.txt"
  exit "$status"
fi
printf 'PASS cases=%s seed=0x%s\n' "$CASES" "$SEED_HEX" > "$OUT/result.txt"
