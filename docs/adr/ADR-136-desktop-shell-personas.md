# ADR-136 — The desktop shell persona: one window manager, four conventions

- **Status:** accepted
- **Date:** 2026-09-11
- **Requirement:** ALET-P2-021 (the GUI rung), REQ-GUI-PERSONA-001
- **Supersedes:** nothing. Extends ADR-084 (managed windows) and ADR-085 (one desktop, three CPUs).

## Context

Aletheia's desktop had exactly one layout. The panel was pinned to the bottom of the scanout by a
constant, the launcher was the first eight cells of it by another constant, the application
buttons followed at a third, and the workspace strip followed those. Every one of those positions
was a `const` compiled into `desktop.rs`, read independently by the painter and by three separate
hit-map functions.

Two problems followed from that.

The first is a usability one. A person arriving at Aletheia arrives with a convention already in
their fingers: a Windows user reaches for the bottom-left corner and expects the tray bottom-right,
a macOS user reaches for a menu bar at the top and for window controls on the LEFT of a title bar,
a GNOME user reaches top-left for activities. Aletheia answered all three with one arrangement and
no way to change it. "Learn ours" is a defensible position for a research kernel and an indefensible
one for an operating system that intends to be used.

The second is a correctness one that the first exposed. The constants did not actually fit. One
application button was eighteen cells — 144 px — and there were three of them, after a 64 px
launcher, followed by four 56 px workspace buttons: 720 px of chrome on a 640 px scanout. Workspace
buttons 3 and 4 were painted past the right edge of the framebuffer and could not be reached with
the pointer at all. Nothing caught it, because no invariant said the chrome had to FIT; the painter
and the hit map agreed with each other about coordinates that the screen did not contain.

## Decision

Make the convention an explicit value, and make reachability an invariant.

`kernel-core/src/persona.rs` holds a pure, allocation-free, arch-neutral policy:

* `ShellPersona` — `Aletheia`, `Windows`, `Macos`, `Gnome`.
* `ChromeMetrics` — the chrome's fixed measurements, supplied by the desktop that owns the
  surfaces rather than baked in, so the same policy serves a different scanout without a second
  copy of the rules.
* `ShellPersona::layout(ChromeMetrics) -> ChromeLayout` — a total function returning the panel
  edge, the panel's y origin, the x origin of each of the three clusters, and which side of a
  title bar carries the window controls.
* `ChromeLayout::hit()` — the ONE geometric answer to "what is under this x", which the desktop's
  three hit maps and its painter now all consult.

The desktop holds the persona and its resolved layout, recomputed once per persona change rather
than per event, so the pointer hot path stays a handful of comparisons (ADR-133's fast lane is
untouched). `Alt+P` cycles the persona. A persona change is a MOVE: the panel surface, its token,
its grid and every managed window survive untouched; only the panel's origin and the cached
geometry change. Nothing is allocated, which matters on a heap that never frees (ADR-063).

Three consequences are deliberate:

1. **`ShellPersona::Aletheia` reproduces the historic layout exactly.** It is the packed order the
   constants encoded. Every GUI proof recorded before personas existed keeps its meaning, and the
   default boot is visually unchanged.
2. **The application button narrows from eighteen cells to fourteen.** 64 + 3×112 + 4×56 = 624 px,
   which fits the 640 px scanout with room to spare. This is the fix for the unreachable workspace
   buttons, and it is what makes the other three conventions expressible at all.
3. **Reachability outranks convention.** When a scanout is too narrow for a persona's preferred
   arrangement, the layout degrades to packed order rather than to an affordance that is painted
   off-screen. A cramped panel is a presentation problem; an unclickable button is a correctness
   one.

The persona is presentation-only. It never becomes window-manager authority: `set_persona` creates
no window, destroys none, focuses none and reorders none. The window manager remains the single
authority over the managed set, exactly as ADR-084 left it.

## The invariants (`persona=8`, proved on all three CPUs at boot)

1. The in-house convention reproduces the historic packed panel layout.
2. Every persona keeps every affordance inside the scanout. *(This is the invariant the old
   eighteen-cell button violated.)*
3. No persona lets two affordances claim the same pixel.
4. Every cluster pixel resolves to the affordance that owns it.
5. The panel is pinned to the edge the persona declares.
6. Cycling forward reaches every persona and returns to the start.
7. Only the macOS convention puts the window controls on the leading edge.
8. A scanout too narrow for a convention degrades to packed order.

The suite measures `persona::LIVE_CHROME`, and a host test asserts that the desktop's own `CHROME`
IS `LIVE_CHROME` — a boot proof about geometry the machine never paints would be no evidence at
all.

## Alternatives considered

**A compile-time feature per persona.** Rejected: it makes the convention a build decision rather
than a user one, and it would have left three of the four layouts unproved on any given image.

**Persona as window-manager state.** Rejected: the window manager owns authority, and a
presentation preference that can reach into it is a second focus authority waiting to happen. The
desktop owns the persona; the window manager never learns of it.

**Per-window personas.** Rejected as premature. The convention a user has in their fingers is a
property of the user, not of a window; mixing conventions within one screen is the confusing case,
not the useful one.

## Consequences

* The GUI can now meet a Windows, macOS or GNOME user where they are, and Aletheia keeps a
  distinct in-house convention as the default rather than imitating any of them.
* `ControlSide` is defined and proved, and the persona declares it, but the title-bar painter and
  its hit map still place controls on the trailing edge unconditionally. Honouring `ControlSide`
  in `wm` is scoped as follow-on work and is recorded in the gap register; until then the macOS
  persona differs from macOS in exactly that respect, and this ADR says so rather than implying
  otherwise.
* Adding a fifth persona requires touching `ShellPersona::ALL`, which `PERSONA_COUNT`, the cycle,
  the labels and invariant 6 all read — so a persona cannot be added without the proof seeing it.
