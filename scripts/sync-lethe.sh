#!/usr/bin/env bash
# sync-lethe.sh — move the upstream Lethe pin forward, deliberately.
#
# Aletheia tracks Lethe by commit (third_party/lethe.pin, docs/LETHE-INTEGRATION.md). This script
# is how that commit moves: it fetches the upstream branch, SHOWS what changed since the current
# pin, and only then rewrites the pin. It is an operator's tool, not a CI gate — it needs the
# network and a fetchable remote, and a gate that needs the internet goes red for reasons that
# have nothing to do with the change under test. `scripts/check-lethe-pin.sh` is the CI half.
#
#   scripts/sync-lethe.sh            # show what moved, then update the pin
#   scripts/sync-lethe.sh --dry-run  # show what moved, change nothing
#
# It never vendors Lethe's source into this tree. Lethe's engine cannot run on the kernel (no
# libc, no TCP, no TLS — see docs/LETHE-INTEGRATION.md), so a vendored copy would be dead weight
# that looks like a capability. What Aletheia tracks is the CONTRACT, and the pin names it.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PIN="$ROOT/third_party/lethe.pin"
DRY_RUN=0
[ "${1:-}" = "--dry-run" ] && DRY_RUN=1

[ -f "$PIN" ] || { echo "FAIL: $PIN is missing"; exit 1; }

field() { sed -n "s/^$1[[:space:]]*=[[:space:]]*//p" "$PIN" | head -1; }
remote="$(field remote)"
branch="$(field branch)"
old="$(field commit)"

echo "==> upstream : $remote ($branch)"
echo "==> pinned at: $old"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# A bare mirror of the single branch: enough to read history, nothing checked out.
git init --quiet --bare "$work/lethe.git"
git --git-dir="$work/lethe.git" remote add origin "$remote"
if ! git --git-dir="$work/lethe.git" fetch --quiet --depth=200 origin "$branch"; then
  echo "FAIL: could not fetch $branch from $remote (network? credentials?)"
  exit 1
fi
new="$(git --git-dir="$work/lethe.git" rev-parse FETCH_HEAD)"

if [ "$new" = "$old" ]; then
  echo "==> already current: the pin is upstream's $branch head"
  exit 0
fi

echo "==> upstream head is $new"
echo
echo "--- what moved since the pin ---"
if git --git-dir="$work/lethe.git" cat-file -e "$old^{commit}" 2>/dev/null; then
  git --git-dir="$work/lethe.git" log --oneline --no-decorate "$old..$new" | sed 's/^/  /'
else
  echo "  (the pinned commit is not in the fetched depth — showing the last 20 upstream commits)"
  git --git-dir="$work/lethe.git" log --oneline --no-decorate -20 "$new" | sed 's/^/  /'
fi
echo "--------------------------------"
echo

if [ "$DRY_RUN" -eq 1 ]; then
  echo "==> --dry-run: the pin was NOT changed"
  exit 0
fi

today="$(date -u +%Y-%m-%d)"
tmp="$(mktemp)"
sed -e "s|^commit *=.*|commit = $new|" -e "s|^taken  *=.*|taken  = $today|" "$PIN" > "$tmp"
mv "$tmp" "$PIN"

echo "==> pin updated: $old -> $new (taken $today)"
echo
echo "Read the commits above before committing this. A pin move is a statement that Aletheia's"
echo "integration contract still matches upstream; if any of those commits changed a behaviour"
echo "docs/LETHE-INTEGRATION.md describes, update that page in the SAME commit."
bash "$ROOT/scripts/check-lethe-pin.sh"
