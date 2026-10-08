# ADR-247 — Console roles: least privilege between the people at one machine

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-SEC-AUTH-002 (new)
**Builds on:** ADR-244 (console accounts), ADR-246 (the lock covers the desktop).

## Context

ADR-244 named its own limit: one role. Every account, once logged in, held every console
capability, and any logged-in account could set any account's password. The production-readiness
audit listed per-user authority as an open row.

## Decision

Three roles, stored as a fifth field of the account line (`name:iterations:salt:hash:role`):

| role | may | may not |
|---|---|---|
| `admin` | everything | |
| `operator` | read, write, flush, run and schedule programs, change the display | reboot, halt, overclock; another account's password; any role |
| `viewer` | read | anything else |

* **Checked before the machine's capabilities.** Every console action goes through one gate,
  `ShellAction::allowed_for(role)`, before `ShellHost::authorize`; a refusal names the action and
  the role (`permission denied: system.halt (not for the operator role)`). The dispatcher gains
  `execute_as(.., role, ..)`; `execute` keeps its signature and runs as `admin`, which is what a
  machine without accounts always was.
* **Accounts are an admin's.** The first account on a machine is an admin (asking for another
  role is refused by name). An admin creates or resets any account and names its role (default:
  the account's current role, else operator). Anyone else may change their own password and
  nothing else, keeping their role, even a viewer, since a password is the account's own.
* **The record stays out of sight.** `ls` and `find` skip `.users`, as the desktop panel does
  since ADR-246; every command naming it is already refused.
* Lines from ADR-244 (no role field) read as `admin`: they were their machine's only account.
  An unknown role in a line makes the line refuse to log in rather than guess.
* `whoami` and the login greeting name the role.

## Evidence

* Host: role parsing and the old-line rule; `roles_limit_what_an_account_may_do` drives one
  machine through an admin creating an operator and a viewer, a viewer refused write, halt and
  any other account, a viewer changing its own password, an operator writing and reading but
  refused halt; the record never appears in `ls`. kernel-core release suite passes.
* console-e2e on all three CPUs (the account it creates now reports `admin`); console fuzz; boot
  gates.

## What this is not

Roles are fixed; there is no per-object permission (an operator may write every object but the
record). Programs a session runs carry no account: the user-mode capability grant (ADR-207) does
not yet depend on who started the program.
