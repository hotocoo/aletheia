#!/usr/bin/env bash
# Machine-readable invariant markers shared by every VM gate (ALET-P2-007, REQ-QUAL-005, ADR-061).
#
# Every kernel suite already ends its section with "[tag] ALL N NAME INVARIANTS HOLD" — a human
# sentence each gate greps for individually. Prose greps answer "did THIS family still say N?" but
# nothing answers two bigger questions: did any family DISAPPEAR from the boot entirely, and did a
# family's count change without anyone telling the gate? These helpers turn the boot log into a
# tag=count MAP and hold it against an expected map declared IN THE GATE, failing on missing,
# unexpected, or changed families — each named. On success the gate prints ONE machine-readable
# line (GATE-MARKERS-V1: ...) that CI can collect without ever parsing prose again.
#
# Portability note: pure POSIX-ish bash, no associative arrays (macOS ships bash 3.2), no jq.
# Comparison is a sorted-line diff, which names every difference in place.

# Extract "tag=count" lines from a boot log supplied ON STDIN.
marker_lines() {
  grep -oE '^\[[a-z0-9-]+\] ALL [0-9]+ [A-Z0-9 -]*INVARIANTS HOLD' \
    | sed -E 's/^\[([a-z0-9-]+)\] ALL ([0-9]+) .*/\1=\2/' \
    | sort
}

# Assert the boot log on STDIN carries EXACTLY the expected family/count map ($1, space-separated
# tag=N pairs). Extra families are a FAILURE too: a new suite must be added to the gate's expected
# map DELIBERATELY, never silently ignored. Prints the GATE-MARKERS-V1 line on success.
markers_assert() {
  local want actual
  want=$(printf '%s\n' "$1" | tr ' ' '\n' | sed '/^$/d' | sort)
  actual=$(marker_lines)
  if [ "$want" != "$actual" ]; then
    echo "FAIL: invariant-family markers differ from this gate's expected map:"
    diff <(printf '%s\n' "$want") <(printf '%s\n' "$actual") | sed 's/^/    /'
    return 1
  fi
  printf 'GATE-MARKERS-V1: %s\n' "$(echo "$actual" | tr '\n' ' ')"
}
