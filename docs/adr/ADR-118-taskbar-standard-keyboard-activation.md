# ADR-118 — Taskbar standard keyboard activation

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-117

## Context

ADR-117 completed taskbar navigation with a desktop-specific `F6` mode, arrows, `Home`, `End`,
`Enter`, and `Escape`. The remaining interaction gap was that the mode did not expose the two
standard keyboard patterns users expect from a selectable strip: `Tab`/`Shift+Tab` traversal and
`Space` activation.

## Decision

While taskbar keyboard navigation is active:

- `Tab` advances to the next taskbar affordance and `Shift+Tab` moves to the previous one.
- `Space` activates the selected affordance using the same desktop/window-manager authority as
  `Enter`.
- These keys are consumed by the desktop only while taskbar navigation is active. Outside that
  mode, their existing application-input behavior is unchanged.

## Invariants

- Taskbar `Tab` navigation follows the same wrapping order as left/right navigation.
- `Shift+Tab` reverses that order without changing focus ownership.
- `Space` and `Enter` invoke exactly the same activation path.
- No taskbar-navigation key is leaked into the focused application while the mode is active.
- Existing `Ctrl+Tab` and `Alt+Tab` window cycling remain unaffected because taskbar mode has
  explicit precedence only while it is active.
