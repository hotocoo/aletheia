# ADR-244 — Console accounts: the console stops being root for whoever reaches it

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-SEC-AUTH-001 (new)
**Builds on:** ADR-044 (interactive console), ADR-089 (storm discipline), ADR-153/236 (entropy sources), ADR-237 (open row "user authentication").

## Context

MATURITY's console row said it plainly: the boot console is a privileged root policy that mints
console and system capabilities at boot; nobody is asked who they are. Anyone with the serial
line, the desktop or the VMware console held every console capability. ADR-237 named it an open
row.

## Decision

`kernel_core::login` plus the console session (all three CPUs, one implementation):

* **Opt-in, then mandatory.** A machine with no account record behaves exactly as before. The
  first `passwd NAME` (asked twice, never echoed) writes the record; from then on every console
  session on that machine, after every reboot, starts locked and answers only `login NAME`.
  `logout` locks it again; `whoami` names who opened it.
* **Salted, slow, standard hashing.** One line per account in the namespace object `.users`:
  `name:iterations:salt:hash`, PBKDF2-HMAC-SHA256 (RFC 8018) with 20 000 rounds over a 16-byte salt
  from the machine's entropy device (virtio-rng, or RDRAND on x86-64). No entropy source, no
  account: `passwd` is refused by name rather than salting from a clock. The iteration count is
  stored per line, so it can be raised without invalidating old records.
* **No oracle in the timing.** Password comparison is constant-time, and a name with no record
  is checked against a fixed dummy at the same cost, so a refusal's duration says nothing about
  which names exist. Every refusal reads the same: `that name and password do not match`.
* **Failures cost time.** Three consecutive failures are free; each further one doubles a wait
  (2 s, 4 s, ... capped at 5 minutes, by the machine's clock) before the next attempt is heard. A
  success clears it.
* **A password never stays visible or stored.** Not echoed, not recorded in history, the editor's
  copy of the line zeroed after use, and Tab completion ignored while one is typed.
* **The record is nobody's to read.** Every command whose arguments name `.users` (a redirect
  included) is refused, so an open session cannot copy the hashes off for an offline guess.
* **A model never types a password.** `aletheiad` classifies `passwd`, `login` and `logout` as
  destructive (a human answers for them); `whoami` is safe.
* Storm discipline: the account check borrows the submitted line; only a line that starts a
  login or a `passwd` allocates.

## Evidence

* Host: PBKDF2 against the RFC 7914 test vector; a record opens for its password only (wrong,
  empty, unknown name, malformed line all refused); upsert keeps other accounts; names refused
  before hashing; back-off doubles to its cap and a success clears it. A transcript test drives
  two sessions over one namespace: open machine, `passwd` with a mismatch refused, second session
  locked, four wrong passwords then a wait, unknown name refused alike, right password opens, three
  ways of reaching `.users` refused, `logout` locks; the password appears in neither output nor
  history. kernel-core release suite 945 passed.
* Live, console-e2e on all three CPUs: session one sets `passwd ada`; after the reboot session two
  starts locked, `cat manifesto` is refused, a wrong password is refused, the right one opens it,
  `cat .users` is refused, and the password string appears nowhere in either session's log.

## What this is not

One role: every account holds the same console capabilities once logged in (no per-user
authority yet), and any logged-in account can set any account's password. The serial line is
plaintext; a password typed over it is visible to whoever can read the wire. There is no account
removal yet other than replacing the record. The desktop's terminal window is the same console
session, so it is locked too; the boot suites and `aletheiad`'s own API are unaffected.

## Consequences

* MATURITY's console row no longer reads "not yet a user-authenticated shell" for a machine with
  an account.
* Per-account capabilities (mint the console's tokens per user at login rather than at boot) is
  the next rung.
