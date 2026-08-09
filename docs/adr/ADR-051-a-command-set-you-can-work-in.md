# ADR-051 — A command set an operator can actually work in

**Status:** Accepted
**Date:** 2026-08-09
**Requirements:** REQ-CON-005 (a working command set over the namespace)
**Closes:** GAPS4 `ALET-P2-041`
**Extends:** ADR-044 (interactive console), ADR-050 (the console parses its input)

---

## Context

ADR-044's console had twelve commands: `help`, `arch`, `uptime`, `mem`, `df`, `ls`, `stat`, `cat`,
`write`, `rm`, `echo`, `halt`. That was the right first set — every one of them exercises a
subsystem that was already proved, and none of them added unproved surface underneath the console.

It is not a set anyone can work in. There is no way to copy an object, rename one, add a line to
one, look at one that is not text, search one, or find a name you half-remember. An operator who
wants to keep a note and a copy of it has to retype the note. The same session cannot even tell you
what it has run.

The report was blunt about it — "the console is too little as an OS" — and it arrived alongside the
input bug ADR-050 fixes. The two are one complaint: a console is the part of this system a person
touches, and it was neither editable nor useful.

---

## Decision

Fifteen commands, chosen by one rule: **each must be a different path through machinery that already
exists**, and none may add a subsystem the console would then be the only proof of.

| Command | The path it exercises |
|---|---|
| `ver` | what this system says about itself, in one place |
| `lsblk` | the block device's geometry — the thing `df` reports space *within* |
| `find PREFIX` | the directory, searched rather than listed |
| `head`, `wc`, `grep` | reading an object and interpreting it as text |
| `hexdump` | reading an object and refusing to interpret it — the case `cat` declines |
| `append` | read-modify-write in ONE replace transaction |
| `touch` | create-if-absent, and the refusal to touch what exists |
| `cp`, `mv` | copy, and copy-then-remove |
| `sync` | the device's flush path, which nothing else reached from the console |
| `history` | the editor's own list, printed |
| `clear`, `reboot` | the terminal, and the platform's reset |

Four of these needed a decision rather than an implementation:

### 1. `touch` never truncates

An object that exists is left alone and its size is reported. There is no modification time to
update, so the only other thing `touch` could do to an existing object is destroy it — and a
harmless-looking command that eats data is the worst kind of defect a command table can carry.

### 2. `mv` is copy-then-remove, in that order, and is NOT atomic

Said out loud because `mv` reads atomic and is not. A crash between the two transactions leaves
**both** names, which is recoverable by hand; the other order loses the data. One-transaction rename
is possible in this filesystem and is not what was built, because it would mean a second directory
mutation path next to `replace`, and this ADR's rule is that the console adds no machinery.

### 3. `append` is a read-modify-write through `replace`

One transaction, so a crash leaves the old contents or the new ones (ADR-035), never a half-appended
object. Appending in place would need the extent to have room, which is a second failure mode for a
command whose entire value is that it is boring.

### 4. A bad numeric argument is refused, never defaulted

`head notes x` says `head: N must be a number`. Quietly reading ten lines because the count would
not parse makes the output an answer to a question nobody asked, which is worse than an error in
exactly the situation where the operator is trying to understand something.

### 5. `reboot` is the platform's reset on all three targets, and admits failure

x86-64 pulses the i8042 reset line, RISC-V calls SBI `SYSTEM_RESET`, aarch64 calls PSCI
`SYSTEM_RESET` over the same `hvc` conduit `smp.rs` already uses to start secondary CPUs. None of
them returns on success. Each **can** fail — a legacy-free PC, a firmware without the optional SRST
extension — and when it does the console says the machine did not restart rather than hanging in a
loop pretending the reboot is in progress. `ShellHost::reboot` returns `bool` for that reason and
defaults to `false`: a target that has not implemented a reset path says so.

### 6. Two facts the machine can now be asked for

`cpu_count` is the MADT's declared processor count on x86-64 (defaulted to one elsewhere), and it is
deliberately the count the *firmware declares*, not the count this kernel has *started* — an
operator asking what the machine has is asking about the machine, and conflating the two would make
a boot that failed to start a core look like a single-processor machine instead of a defect.
`mem` now reports MiB as well as frames, because an operator thinks in the first and the allocator
counts in the second.

### 7. Ten new live invariants, on every target

`console_suite` grew from 30 to 40. They are behaviors rather than smoke tests: a copy is an
independent object (proved by writing the original afterwards and reading both), a rename leaves no
old name, `append` keeps what was there, `touch` does not truncate, `grep` finds the line that
matches and not the line that does not, `hexdump` shows the bytes `cat` refuses, and — swept over
the whole command table rather than sampled — **every command that needs an argument refuses to run
without one**. Six are added to `conformance.sh`'s cross-target contract: a target whose `mv` left
both names would be a different operating system wearing the same command table.

`console-e2e.sh` now drives the working set at a running machine on all three targets and asserts
across a reboot that what `append`, `cp` and `mv` produced is on the medium rather than in the
session.

---

## Consequences

**Measured.** 27 commands. `[console] ALL 40 CONSOLE INVARIANTS HOLD` on aarch64, RISC-V and x86-64;
`CONFORMANCE: PASS` at 118 named cross-target behaviors (was 112); `CONSOLE-E2E: PASS` on all three
targets with the new commands driven at a live console and their effects re-read after a reboot.

**Still not a shell.** No pipes, no redirection, no globbing, no variables, no scripting, no exit
status. Commands take positional words and print lines. Adding a grammar means adding a parser with
its own refusals, and this console's whole claim is that it adds reach without adding unproved
surface.

**Still kernel-space.** Every command drives the kernel's own objects directly. `ver` says so on the
machine itself, because the person most likely to over-claim about this system is the one sitting in
front of it.

**`clear` is the one terminal assumption.** It writes `ESC [ 2 J ESC [ H`. A terminal that ignores
escape sequences shows nothing rather than garbage, which is the acceptable direction to be wrong in.

**Not claimed — directories.** The namespace is flat, `/` is still a refused character in a name,
and `find` searches by prefix because a prefix is what a flat namespace has instead of a path.

## Alternatives considered

**A shell grammar (pipes, redirection).** Rejected for this wave. Every pipe stage needs a place to
put bytes, and that is a buffering policy with its own bounds and its own refusals — a subsystem,
not a command.

**Aliases (`free` for `mem`, `dir` for `ls`).** Rejected: `help` lists the table, the live suite
asserts the table and the dispatcher agree, and an alias is a name that is reachable and
undocumented unless the table carries it twice.

**`reboot` as an `Outcome` variant like `Halt`.** Rejected. Halting is a decision the *session*
returns to its caller because the caller owns the exit-code contract; resetting is something the
*platform* does and does not return from. Modelling it as an outcome would have every target's
console loop grow a branch for a case that never comes back.
