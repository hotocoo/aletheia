#!/usr/bin/env bash
# The assembly/Rust boundary, the unsafe surface, and the invariant index are DOCUMENTED
# (ALET-P3-001/002/003) — and like every claim in this repository, documentation that can
# silently drift is not documentation. This gate regenerates each inventory FROM THE TREE and
# holds the docs against it:
#
#   [1] every .rs file with an asm!/naked_asm!/global_asm! site is listed in
#       docs/ASM-BOUNDARY.md with its exact per-file count, and no stale rows remain;
#   [2] docs/UNSAFE-AUDIT.md per-crate counts match the tree exactly (token occurrences on
#       code lines — lines whose first non-whitespace character is not '//' — as stated there);
#   [3] every '## INV-*' section of docs/INVARIANT-CONTRACTS.md has a row in
#       docs/INVARIANTS-INDEX.md, and the index names nothing the contract lacks.
#
# Exit 0 iff all three hold. Pure bash + coreutils, like its sibling checkers.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
fail=0

ASM_DOC="$ROOT/docs/ASM-BOUNDARY.md"
UNSAFE_DOC="$ROOT/docs/UNSAFE-AUDIT.md"
INDEX_DOC="$ROOT/docs/INVARIANTS-INDEX.md"
CONTRACTS="$ROOT/docs/INVARIANT-CONTRACTS.md"

for f in "$ASM_DOC" "$UNSAFE_DOC" "$INDEX_DOC" "$CONTRACTS"; do
  if [ ! -f "$f" ]; then echo "FAIL: missing document: $f"; fail=1; fi
done
if [ "$fail" = 1 ]; then exit 1; fi

echo "== [1] assembly/Rust boundary inventory"
tree_asm=$(grep -rnE '(^|[^a-zA-Z_])(global_asm|naked_asm|asm)!' --include='*.rs' \
      kernel/src kernel-core/src kernel-x86_64/src kernel-riscv64/src \
  | awk -F: '{ l=$2; sub(/^[ \t]+/, "", l); if (l !~ /^\/\//) print $1 }' \
  | sort | uniq -c | awk '{print $2" "$1}' | sort)
doc_asm=$(grep -E '^\| .[^|]*. \| [0-9]+ \|' "$ASM_DOC" \
  | sed -E 's/^\| .([^|]+). \| ([0-9]+) \|.*/\1 \2/' | sed 's/ *$//' | sort)
n_tree_files=$(echo "$tree_asm" | grep -c .)
if [ "$n_tree_files" -eq 0 ]; then echo "FAIL: no asm sites found in tree?"; fail=1; fi
missing=$(comm -23 <(echo "$tree_asm") <(echo "$doc_asm"))
stale=$(comm -13 <(echo "$tree_asm") <(echo "$doc_asm"))
if [ -n "$missing" ]; then
  echo "FAIL: asm files in the tree but NOT in docs/ASM-BOUNDARY.md:"; echo "$missing"; fail=1
fi
if [ -n "$stale" ]; then
  echo "FAIL: rows in docs/ASM-BOUNDARY.md that no longer match the tree:"; echo "$stale"; fail=1
fi
total_tree=$(echo "$tree_asm" | awk '{s+=$2} END {print s+0}')
total_doc=$(echo "$doc_asm" | awk '{s+=$2} END {print s+0}')
if [ "$total_tree" != "$total_doc" ]; then
  echo "FAIL: total site count drifted: tree=$total_tree doc=$total_doc"; fail=1
fi
if [ "$fail" = 0 ]; then
  echo "  PASS: $n_tree_files files / $total_tree sites inventoried exactly"
fi

echo "== [2] unsafe audit inventory"
count_unsafe() {
  find "$ROOT/$1/src" -name '*.rs' 2>/dev/null | while read -r f; do cat "$f"; done \
    | awk '{ l=$0; sub(/^[ \t]+/, "", l); if (l !~ /^\/\//) print l }' \
    | grep -oE '\bunsafe\b' | wc -l | tr -d ' '
}
doc_counts=$(grep -E '^\| +[a-z][a-z0-9_-]+ +\| [0-9]+ +\|' "$UNSAFE_DOC" \
  | sed -E 's/^\| +([a-z][a-z0-9_-]+) +\| ([0-9]+) +\|.*/\1 \2/')
while read -r crate doc_n; do
  if [ -z "${crate:-}" ]; then continue; fi
  tree_n=$(count_unsafe "$crate")
  if [ "$tree_n" != "$doc_n" ]; then
    echo "FAIL: unsafe count for $crate: doc says $doc_n, tree has $tree_n"; fail=1
  fi
done <<EOF
$doc_counts
EOF
if ! grep -qE 'owner|Owner|review|Review' "$UNSAFE_DOC"; then
  echo "FAIL: docs/UNSAFE-AUDIT.md names no ownership/review policy"; fail=1
fi
if [ "$fail" = 0 ]; then
  n_crates=$(echo "$doc_counts" | grep -c .)
  echo "  PASS: unsafe counts for $n_crates crates match the tree; ownership policy present"
fi

echo "== [3] invariant index parity"
contract_ids=$(grep -oE '^## INV-[A-Z0-9-]+' "$CONTRACTS" | sed 's/^## //' | sort)
index_ids=$(grep -oE 'INV-[A-Z0-9-]+' "$INDEX_DOC" | sort -u)
missing_inv=$(comm -23 <(echo "$contract_ids") <(echo "$index_ids"))
extra_inv=$(comm -13 <(echo "$contract_ids") <(echo "$index_ids"))
if [ -n "$missing_inv" ]; then
  echo "FAIL: contract sections with NO index row:"; echo "$missing_inv"; fail=1
fi
if [ -n "$extra_inv" ]; then
  echo "FAIL: index rows naming sections the contract does not have:"; echo "$extra_inv"; fail=1
fi
if [ "$fail" = 0 ]; then
  echo "  PASS: $(echo "$contract_ids" | grep -c .) contract families indexed"
fi

if [ "$fail" = 1 ]; then
  echo "----------------------------------------"
  echo "BOUNDARY DOCS: FAIL (the docs no longer describe the tree)"
  exit 1
fi
echo "----------------------------------------"
echo "BOUNDARY DOCS: PASS (assembly boundary, unsafe audit and invariant index match the tree)"
