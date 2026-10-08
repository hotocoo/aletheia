# ADR-250 — Operators own what they create

**Status:** Accepted (2026-10-09)
**Requirements:** REQ-SEC-AUTH-003 (new)
**Builds on:** ADR-244 (accounts), ADR-247 (roles).

## Context

ADR-247 gave accounts fixed roles but said what it left out: no per-object permission, so any
operator could overwrite or remove any other operator's objects. The production-readiness audit
listed per-file permissions as an open row.

## Decision

* **An ownership record**, `.owners` (`name:owner` lines), as private as `.users`: no command reads,
  lists or names it, the desktop panel does not show it, and the refusal says what it is.
* **Operators own what they create.** A `write`, `append`, `touch`, `cp` or `mv` that creates an
  object records the operator as its owner. An operator may change (`write`, `append`, `rm`, `mv`
  from or to, `cp` onto) only an object it owns or one nobody owns, and is refused by name
  otherwise (`permission denied: plan belongs to ada`). Removing an object drops its line; `mv`
  moves ownership with the name.
* **Reading is not owning.** Every account that may read may read every object; ownership governs
  change only.
* **Admins and machines without accounts pay nothing.** They change anything and record nothing,
  so the console storm's per-command costs and every existing gate are unchanged. A line an admin's
  removal left behind names no live object and is ignored.
* Objects that existed before ownership, or that an admin created, are owned by nobody: shared.

## Evidence

* Host: record helpers (set, move, drop); `operators_own_what_they_create` drives an admin and two
  operators over one namespace: an operator's creation is refused to the other through `write`,
  `rm`, `mv`, `cp` and `append`, still readable by it; a shared object stays shared; the record is
  refused and unlisted; after an admin removes the object the name is free again.
* kernel-core release suite, boot gates on all three CPUs, console and console-fuzz gates.

## What this is not

No read permission, no groups, no `chown` command (an admin replaces or removes the object), and
programs a session runs are not subject to ownership (they can only read).
