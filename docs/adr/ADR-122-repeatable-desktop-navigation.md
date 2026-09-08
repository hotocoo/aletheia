# ADR-122 — Desktop navigation honors key repeat

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-117, ADR-118, ADR-120, ADR-121

## Decision

The shared desktop treats Linux input key value `2` (autorepeat) as navigation input anywhere a
navigation key already accepts an initial press. Start-menu Up/Down and taskbar Home/End/Left/Right
therefore continue moving the retained selection while a key is held.

Activation remains press-only: Enter, Space, number accelerators, and desktop mode-entry actions do
not become repeatable launches or window mutations merely because the input device emits repeats.

The rule is centralized in `is_key_press_or_repeat`, so navigation cannot accidentally diverge
between the start menu and taskbar. Repeated navigation is still consumed by the desktop and is
never delivered to the focused application while the corresponding desktop UI owns the gesture.

## Invariants

- key value `1` and repeat value `2` are navigation events;
- key release value `0` is never navigation;
- non-key events are never navigation;
- repeated navigation changes presentation state only;
- activation remains a single press and keeps existing lifecycle authority;
- the desktop hot path remains allocation-free.

## Verification

- `cargo test --lib desktop::tests` — 19 passed;
- `cargo test --test wm` — 43 passed;
- `cargo test --test experience_gui` — 2 passed;
- `git diff --check` — passed.
