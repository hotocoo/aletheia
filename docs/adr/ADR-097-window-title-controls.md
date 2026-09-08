# ADR-097 — Window title bars expose lifecycle controls

**Status:** accepted · **Date:** 2026-09-08 · **Builds on:** ADR-084, ADR-093, ADR-094, ADR-095, ADR-096

## Context

The desktop already supported minimize, maximize/restore and close, but two of those operations
were keyboard-only. A GUI should expose the same lifecycle controls where users already look: the
window title bar.

## Decision

Managed windows paint three fixed title-bar controls: minimize (`-`), maximize/restore (`+`), and
close (`x`). `WindowManager::hit_at` uses the same fixed-width geometry, and `press` invokes the
existing manager operations using the manager-held owner token and input session.

The compositor remains the sole focus authority. The controls are routing commands, not separate
surfaces or input queues. A direct maximize/restore click reuses the existing geometry contract;
the live desktop synchronizes the terminal grid after the click.

## Consequences

- Minimize, maximize/restore and close are discoverable without memorizing shortcuts.
- Painted and clickable control regions share one geometry definition.
- No new focus authority, window surface, or per-tick repaint path is introduced.
