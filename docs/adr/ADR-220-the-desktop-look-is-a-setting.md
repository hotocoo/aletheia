# ADR-220 — The desktop's look is a setting

**Status:** Accepted (2026-10-07)
**Requirements:** REQ-CON-012 (new)
**Builds on:** ADR-219 (the machine keeps its settings), ADR-136 (the shell personas,
`kernel-core/src/persona.rs`: Aletheia's own layout and Windows-, macOS- and GNOME-like ones).

## Context

The desktop already had four looks - Aletheia's own and three that put the panel, launcher and
window controls where a Windows, macOS or GNOME user expects them - but the only way to choose was
`Alt+P` on the desktop keyboard, and the choice was gone at the next boot. A person who wants the
in-house look, or the one they already know, should choose it once.

## Decision

* **`persona [NAME]`** at the console shows the look, or sets it - `aletheia`, `windows`, `macos`,
  `gnome`, by the labels the personas already carry (`ShellPersona::from_label`) - and keeps it in
  the `settings` object as `persona=NAME`. A machine without a live desktop refuses by name.
* **The console puts it back at start** with the other settings (ADR-219), naming it. An unknown
  name in the file is a skipped line, not a failure.
* `Alt+P` still cycles the look for the session; the console's command is the one that is kept.

## Proof

* Host: `persona=` round-trips through the settings object; an unknown persona is skipped
  (`kernel-core/src/settings.rs`).
* Live (`scripts/console-e2e.sh`): aarch64 and riscv64 set `persona macos`, and the second boot of
  the same disk prints `settings: persona macos` and reports the macOS-like look; the GPU-less
  x86-64 console refuses it by name.

## Non-claims

* The personas move the panel, launcher and window controls; they are layouts, not themes - no
  per-persona fonts, colours or widgets yet.
