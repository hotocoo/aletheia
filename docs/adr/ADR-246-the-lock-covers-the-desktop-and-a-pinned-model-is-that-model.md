# ADR-246 — The lock covers the desktop, and a pinned model is that model

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-SEC-AUTH-001, REQ-AI-017
**Builds on:** ADR-137 (file panel), ADR-157 (browser window), ADR-244 (console accounts), ADR-245 (v0.7.0).

## Context

A review of v0.7.0 after publication found two defects in what it shipped.

1. **The console lock had a side door.** ADR-244 checked the lock on typed lines only. The
   console loop also serves the desktop on every idle turn: a click in the file panel opens an
   object and prints it through the console, and the browser window's requests are navigated. A
   locked console did both, so anyone at the desktop could read any object without logging in.
   Writing the test for the fix found a second door inside the first: the loop's very first
   settle (before the first prompt) served the panel before the lock had been decided, so a click
   latched during boot was opened on a locked machine.
2. **A pinned model could be served with other weights.** ADR-245 fixed `model pull` to count
   only the archive's own snapshot, but discovery still fell back to any cached snapshot of the
   same repo. A machine with v2 cached therefore resolved the v4 manifest to v2's weights and
   `model serve` served them under v4's name; identity is checked by `serve_id`, not by weights.

## Decision

* **The lock is decided before the desktop is served, and covers it.** The session decides
  whether it is locked before the first settle. While locked, the loop takes the browser's
  request and drops it, and calls the service hook with a new phase, `Locked`, in which the file
  panel consumes the click and reads, lists and prints nothing. Neither request is replayed after
  a later login.
* **The account record is not the desktop's.** The panel never lists `.users` and refuses to open
  it, like every console command.
* **A manifest that pins an archive is satisfied by that snapshot only.** Discovery no longer
  falls back to another snapshot of the same repo for such a manifest; the model reads as absent
  and `model pull` fetches the right one. Manifests without an archive pin keep the old lookup.

## Evidence

* Host: `a_locked_console_serves_the_desktop_nothing` drives the real loop with the file panel's
  own service and a click on an object latched on every turn: before login nothing is printed,
  every service turn is `Locked`, no browser page is shown; after login the panel does not list
  `.users` and refuses to open it. It failed on the first-settle door before the ordering fix.
  `an_older_snapshot_does_not_make_a_pinned_model_present` puts an older snapshot in a cache and
  checks the pinned model reads absent until its own snapshot exists.
* This workstation now holds v4 in the snapshot the manifest pins; `aletheiad model status`
  reports `integrity: verified (sha256 matches the pinned manifest)`.
* Boot gates on all three CPUs, console, vinput, keyboard and desktop gates rerun.

## Consequences

* v0.7.0's VMware package carries the side door; v0.7.1 is cut from this change.
