# ADR-084 — Windows are a managed set, not one privileged surface

* Status: accepted
* Date: 2026-09-03
* Register: ALET-P2-021 (graphics / compositor), REQ-GFX-007
* Supersedes nothing; extends ADR-077 (composition contract), ADR-079 (input session),
  ADR-080 (real devices, the live desktop), ADR-083 (the terminal window)

## Context

After ADR-083 the machine had a desktop with exactly one window in it. That window could be
focused, typed into and dragged, and the code that decided so lived in `kernel-x86_64`'s
`desktop.rs`: it asked the terminal's grid whether a press was in its title band, raised the one
surface it knew about, and moved it by hand. Three things were missing, and they were the same
thing:

* **A second application could not exist.** Everything the desktop did was written for the id
  `WINDOW`. A second surface would have needed a second copy of that reasoning.
* **Nothing could be closed.** A window's lifecycle ended only when the machine halted. "Close"
  is not a hide: it is the point where a surface, its input queue and its owner token must die
  together, and where focus must go somewhere honest.
* **Two authorities decided one click.** `vinput::route_pointer_batch` called
  `Compositor::focus_at` on every left press, so the surface under the pointer was focused
  BEFORE anything could observe that the pointer was over a close box.

The register named the gap in exactly these words: "one window with no resize/close/second
application".

## Decision

`kernel-core/src/wm.rs` — a window manager over the composition contract, arch-neutral and
proved on every CPU.

* **The manager owns the tokens.** `WindowManager::open` mints the surface, places it, and keeps
  the owner token. "Which principal may move, raise or close this window" has one answer. An id
  the manager does not hold is refused `UnknownWindow` and counted; a duplicate is
  `DuplicateWindow`; the ceiling is `TooManyWindows`. A window that could not be placed is
  detached again rather than left as an unreachable minted id.
* **Chrome is geometry, and the geometry is the painter's.** `textgrid` gained the close box it
  paints and the predicate `has_close_box(width)`; `wm::hit_at` classifies a window-local point
  through the SAME constants. Painted and clickable cannot disagree, and a window too narrow to
  carry a name beside a close box carries no close box at all — the alternative is chrome that
  is nearly all close box, where every press near the top destroys the window.
* **A press is a decision, reported.** `press` walks the compositor's own z-order front to back
  with the compositor's own visible-rect math (what is clipped away cannot be clicked), and
  returns `Closed`, `Dragging`, `Focused` or `Empty`. It changes no pixel: focus, z-order and
  drag are routing.
* **Close is a lifecycle.** The window is detached — surface, queue and token die together —
  and focus falls to the topmost survivor the manager owns, or is CLEARED when none is left, so
  the next keystroke is refused `NoFocus` rather than routed at a corpse. A re-opened id is a
  NEW window: fresh token, empty queue, dead old token.
* **One authority per click.** `vinput::route_pointer_motion` moves the cursor (the session's own
  plane) and hands the press back undecided. `route_pointer_batch` keeps its ADR-080 behaviour
  for the boot suites; the live desktop uses the motion route and lets the manager decide.

The x86-64 live desktop now runs TWO managed windows on that contract: the terminal (ADR-083)
and a system MONITOR that reports what the machine knows about itself — free frames, device
events, keystrokes posted, drops, refusals, windows open and closed, drags, focus.

**The monitor carries no clock, deliberately.** A panel that repaints once a second is a compose,
a TRANSFER and a FLUSH every second forever on a machine where nothing happened — it would
quietly end ADR-080's quiet desktop. It repaints when a FACT CHANGES: the pump compares every number the panel prints, whole (not a hash, so no two different truths can agree by accident), and an idle machine still costs two used-ring reads
and one damage check per tick.

The wallpaper panel stays a plain surface, not a window: no chrome, no focus, no close. A press
on it is a press on empty desktop, which is what a user means by it.

## Consequences

* New boot-gate family on all three targets: `[wm] ALL 12 WINDOW-MANAGER INVARIANTS HOLD`
  (marker `wm=12`, boot fails `720 + i`); `textgrid` grows 6 -> 7 for the painted close box.
* Six new cross-CPU conformance behaviours (178 -> 185, counting the textgrid addition).
* Host proofs: `kernel-core/tests/wm.rs` (11 tests) beside the boot suite — partially-off
  windows clicked only where visible, a window closed mid-drag, FocusLost across a press,
  queues dying with their window, the ceiling under a long open/close story, and the motion
  route proved NOT to take the click decision.
* The live gate `scripts/vinput-e2e.sh` grows 22 -> 30 checks: the desktop comes up with two
  managed windows, a click on the second focuses it, a keystroke then queues behind it while the
  console sees nothing, the CLOSE BOX ends that window, focus falls to the surviving terminal,
  and the keyboard types at that terminal again.
* `InputFacts` gained `windows`, `closes` and `drags`, and now reports the FOCUSED surface's
  backlog rather than the terminal's — with more than one window, "what has been delivered that
  nobody has read" is a question about the window the keystrokes are going to.
* The x86-64 heap grew 8 -> 12 MiB (the ADR-072 posture on the DT targets). This heap never
  frees: the new suite's desktops and the second resident window's pixels stay for the life of
  the boot, and at 8 MiB the vt-d gate's page tables were the allocation that found the ceiling.

## Named non-claims

* **No resize.** A window's grid and its surface are allocated once (ADR-063's heap never
  frees); resizing means reallocating backing on that heap, which is its own wave.
* **No minimise, no maximise, no window list, no keyboard focus cycling** (alt-tab): focus moves
  by pointer only.
* **No second live desktop.** aarch64 and RISC-V prove the manager in their boot suites; only
  x86-64 installs a timer-pumped desktop (the ADR-080 boundary, unchanged).
* **No application beyond the two the kernel installs.** The monitor is a kernel-side panel, not
  a user-mode program: components do not yet own windows.
* **No close from the keyboard, no confirmation, no unsaved-state protocol.** Close is
  immediate, and what a window held is gone with it.
