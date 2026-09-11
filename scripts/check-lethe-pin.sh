#!/usr/bin/env bash
# check-lethe-pin.sh — the upstream Lethe pin is well-formed and described.
#
# Aletheia tracks Lethe (the browser written as Aletheia's native one) by COMMIT rather than by
# "latest", because "latest" is not a thing a proof can be written against. This gate refuses a
# pin that has rotted into something nobody can act on:
#
#   [1] the pin file exists and carries all four fields
#   [2] the commit is a full 40-hex object name (an abbreviated sha is ambiguous forever)
#   [3] the remote is an https:// or ssh:// URL, not a local path that only one machine has
#   [4] the date is a real ISO date and is not in the future
#   [5] docs/LETHE-INTEGRATION.md exists and names the pin file, so the pin cannot outlive its
#       own explanation
#
# It deliberately does NOT contact the network. A gate that needs the internet is a gate that goes
# red for reasons that have nothing to do with the change under test. Refreshing the pin against
# the real upstream is `scripts/sync-lethe.sh`, which an operator runs.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PIN="$ROOT/third_party/lethe.pin"
DOC="$ROOT/docs/LETHE-INTEGRATION.md"
fail=0

echo "== [1] the pin file exists and carries every field"
if [ ! -f "$PIN" ]; then
  echo "  FAIL: $PIN is missing"
  echo "----------------------------------------"
  echo "LETHE PIN: FAIL (no pin — the tree does not say which Lethe it tracks)"
  exit 1
fi

field() { sed -n "s/^$1[[:space:]]*=[[:space:]]*//p" "$PIN" | head -1; }
remote="$(field remote)"
branch="$(field branch)"
commit="$(field commit)"
taken="$(field taken)"

for name in remote branch commit taken; do
  if [ -z "$(field "$name")" ]; then
    echo "  FAIL: the pin has no '$name' field"; fail=1
  fi
done
[ "$fail" -eq 0 ] && echo "  PASS: remote, branch, commit and taken are all present"

echo "== [2] the commit is a full 40-hex object name"
if printf '%s' "$commit" | grep -qE '^[0-9a-f]{40}$'; then
  echo "  PASS: commit $commit"
else
  echo "  FAIL: commit '$commit' is not a full 40-hex object name"; fail=1
fi

echo "== [3] the remote is fetchable from any machine"
if printf '%s' "$remote" | grep -qE '^(https://|ssh://|git@)'; then
  echo "  PASS: remote $remote"
else
  echo "  FAIL: remote '$remote' is not an https://, ssh:// or git@ URL"; fail=1
fi

echo "== [4] the date is real and is not in the future"
if printf '%s' "$taken" | grep -qE '^[0-9]{4}-[0-9]{2}-[0-9]{2}$'; then
  today="$(date -u +%Y-%m-%d)"
  if [ "$taken" \> "$today" ]; then
    echo "  FAIL: the pin claims to have been taken on $taken, which is after today ($today)"; fail=1
  else
    echo "  PASS: taken $taken"
  fi
else
  echo "  FAIL: taken '$taken' is not an ISO yyyy-mm-dd date"; fail=1
fi

echo "== [5] the pin is described by docs/LETHE-INTEGRATION.md"
if [ ! -f "$DOC" ]; then
  echo "  FAIL: docs/LETHE-INTEGRATION.md is missing — a pin with no explanation is a mystery"; fail=1
elif grep -q 'third_party/lethe.pin' "$DOC"; then
  echo "  PASS: the integration contract names the pin file"
else
  echo "  FAIL: docs/LETHE-INTEGRATION.md does not mention third_party/lethe.pin"; fail=1
fi

echo "----------------------------------------"
if [ "$fail" -eq 0 ]; then
  echo "LETHE PIN: PASS (the tree names exactly which Lethe it tracks, and explains what that means)"
else
  echo "LETHE PIN: FAIL (the pin has rotted — see the failures above)"
  exit 1
fi
