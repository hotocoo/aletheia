# ADR-106 — Desktop Uptime in the Taskbar

## Status

Accepted.

## Context

The desktop taskbar already exposes managed-window state and focus, but it gives no compact
indication that the desktop itself has a live, advancing machine clock. A status indicator must
not introduce a wall-clock dependency, a timer-driven repaint loop, or a second source of truth.

## Decision

The taskbar displays machine uptime in whole seconds using the kernel's existing `Hal` timer.
The value is part of the taskbar's existing equality signature, so the taskbar repaints only when
the displayed second changes or window/focus state changes. If a target deliberately exposes an
uncalibrated timer frequency of zero, uptime remains `0s` rather than inventing a time unit.

## Consequences

- The GUI gets a useful live status value without an RTC or timezone policy.
- No new timer or background task is introduced.
- The taskbar remains compositor-owned and bounded.
- Tests and target implementations can continue to use their existing timer seam.
