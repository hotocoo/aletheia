#!/usr/bin/env bash
# Machine-check the maintained security-boundary inventory (ALET-P2-029..031, ADR-091).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DOC="$ROOT/docs/THREAT-MODEL.md"
[ -f "$DOC" ] || { echo "FAIL: threat model missing"; exit 2; }
grep -q '^## 1\. Adversary and trust assumptions$' "$DOC" || { echo "FAIL: adversary section missing"; exit 1; }
grep -q '^## 2\. Security-boundary inventory$' "$DOC" || { echo "FAIL: boundary inventory missing"; exit 1; }
grep -q '^## 3\. Security versus denial of service$' "$DOC" || { echo "FAIL: security/DoS distinction missing"; exit 1; }
grep -q '^## 4\. Maintained evidence map$' "$DOC" || { echo "FAIL: evidence map missing"; exit 1; }
ids="$(grep -oE '\| B-[0-9]{2} \|' "$DOC" | tr -d '| ' | sort)"
[ -n "$ids" ] || { echo "FAIL: no boundary IDs"; exit 1; }
[ "$(printf '%s\n' "$ids" | uniq -d | wc -l | tr -d ' ')" -eq 0 ] || { echo "FAIL: duplicate boundary ID"; exit 1; }
fail=0
while IFS= read -r path; do
  [ -n "$path" ] || continue
  if [ ! -e "$ROOT/$path" ]; then
    echo "FAIL: missing evidence path: $path"
    fail=1
  fi
done < <(grep -oE '`[A-Za-z0-9_./-]+`' "$DOC" | tr -d '`' | grep -E '^(aletheia|kernel[^ ]*|docs|scripts)/' | sort -u)
[ "$fail" -eq 0 ] || exit 1
echo "THREAT MODEL: PASS (boundary inventory, evidence paths, and security/DoS distinction are maintained)"
