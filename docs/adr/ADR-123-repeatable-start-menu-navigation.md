# ADR-123: Repeatable start-menu navigation

## Status

Accepted

## Context

ADR-122 made desktop navigation consume Linux key-repeat events so held directional navigation
remains responsive. The start menu still treated Home/End and number-row selection as press-only,
which made those navigation affordances inconsistent with the rest of the keyboard navigation
contract.

## Decision

The compositor-owned start menu accepts both Linux key value `1` (initial press) and value `2`
(repeat) for Home, End, and its bounded `1`-`9` selection accelerators. Enter remains press-only
because it activates a command rather than navigating.

## Consequences

- Holding Home or End keeps the menu at its bounded first/last command without leaking the key to
  the focused application.
- Repeated number-row input continues to select the same bounded command without activating it.
- Activation semantics remain press-only, avoiding repeated launches or window mutations.
- The policy remains centralized in `is_key_press_or_repeat` and is covered by the desktop unit
  tests.
