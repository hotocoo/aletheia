#!/usr/bin/env bash
# ALET-P2-006 / REQ-QUAL-008: release reproducibility gate.
# Build the exact same release package twice and require byte-identical package bytes and digest
# metadata. The normal release script still boots packaged disks; this gate isolates reproducibility.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="dev"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    -h|--help) echo "usage: scripts/reproducible-release.sh [--version dev|vX.Y.Z]"; exit 0 ;;
    *) echo "FAIL: unknown argument: $1"; exit 2 ;;
  esac
done

TMP="$(mktemp -d "${TMPDIR:-/tmp}/aletheia-repro.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

echo "==> reproducibility build 1"
"$ROOT/scripts/release-vmware.sh" --version "$VERSION" --no-verify --out "$TMP/one"
echo "==> reproducibility build 2"
"$ROOT/scripts/release-vmware.sh" --version "$VERSION" --no-verify --out "$TMP/two"

ONE="$TMP/one/aletheia-$VERSION-x86_64-vmware.zip"
TWO="$TMP/two/aletheia-$VERSION-x86_64-vmware.zip"
ONE_SHA="$(shasum -a 256 "$ONE" | awk '{print $1}')"
TWO_SHA="$(shasum -a 256 "$TWO" | awk '{print $1}')"
echo "build 1: $ONE_SHA"
echo "build 2: $TWO_SHA"

if ! cmp -s "$ONE" "$TWO"; then
  echo "REPRODUCIBILITY: FAIL (same source/toolchain/config produced different release bytes)"
  # Name WHICH file drifted. A digest that differs tells you the package is not reproducible; it
  # does not tell you whether the cause is a compiler, an image writer or a timestamp, and the two
  # builds are gone by the time anyone reads the log.
  if command -v unzip >/dev/null 2>&1; then
    unzip -qo "$ONE" -d "$TMP/one-x" && unzip -qo "$TWO" -d "$TMP/two-x"
    ( cd "$TMP/one-x" && find . -type f | sort ) > "$TMP/one-list"
    ( cd "$TMP/two-x" && find . -type f | sort ) > "$TMP/two-list"
    if ! cmp -s "$TMP/one-list" "$TMP/two-list"; then
      echo "  the two packages do not even contain the same files:"
      diff "$TMP/one-list" "$TMP/two-list" | head -20
    fi
    while IFS= read -r f; do
      if ! cmp -s "$TMP/one-x/$f" "$TMP/two-x/$f"; then
        echo "  DIFFERS: $f ($(wc -c < "$TMP/one-x/$f") vs $(wc -c < "$TMP/two-x/$f") bytes)"
      fi
    done < "$TMP/one-list"
  fi
  exit 1
fi
if ! cmp -s "$TMP/one/aletheia-$VERSION-x86_64-vmware.zip.sha256" "$TMP/two/aletheia-$VERSION-x86_64-vmware.zip.sha256"; then
  echo "REPRODUCIBILITY: FAIL (zip digest metadata differs)"
  exit 1
fi

echo "REPRODUCIBILITY: PASS (byte-identical release package)"
