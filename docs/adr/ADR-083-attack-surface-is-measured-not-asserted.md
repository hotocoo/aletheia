# ADR-083 — Attack surface is measured, not asserted

**Status:** Accepted (2026-09-12)
**Requirements:** REQ-SEC-002 (new)
**Builds on:** ADR-056 (the honesty rule), ADR-003 (adversarial security-behaviour regressions),
ADR-067 (the supply chain is verified, live, recorded).

## Context

`scripts/comparative-bench.sh` made "faster than Linux" a measurable statement. Nothing in this
repository made "safer than Linux" measurable, and until something does, every security sentence
here is an adjective.

That gap is worse than it looks, because security is the easiest property in systems software to
claim and the hardest to measure. The standard move — compare your design story against somebody
else's CVE count — is available, cheap, and worthless. This repository's own honesty rule
(ADR-056) forbids exactly that shape of argument everywhere else; it should not get an exemption
on the one topic where the temptation is strongest.

## Decision

`scripts/security-surface.sh` measures a small number of things that are **countable on both
systems from primary sources**, names the threat model it is about, and refuses to produce a
verdict.

### The threat model, stated up front

> Unprivileged code already executing on the machine, in user space, trying to obtain authority it
> was not granted.

Not physical access. Not a malicious hypervisor. Not supply chain — ADR-067 covers that separately.
Not network-remote attackers, because Aletheia's network stack is not exposed the way a
general-purpose OS's is and the comparison would be dishonest. One attacker, named, so a reader can
tell what a number is evidence *for*.

### Primary sources on both sides

Aletheia's syscall surface is counted from its own ABI decode table (`kernel-core/src/syscall.rs`),
not from a document that could drift. Linux's is counted from the kernel tree's own
`arch/x86/entry/syscalls/syscall_64.tbl` at a pinned tag — fetched, not cited from a blog post. The
column SKIPs loudly when offline rather than falling back on a remembered number.

## Result (2026-09-12, `docs/evidence/sec001`)

| | Aletheia | Linux 6.12 |
|---|---|---|
| syscalls exposed to user space | 11 | 375 |
| of which gated on a named object capability | 8 | 0 (model differs) |
| privileged lines of code | 39,984 counted Rust | ~40M cited C |
| privileged code in a memory-safe language | yes, except 806 `unsafe` (2.02%) | no |

Linux exposes **34.1x** as many entry points. Of Aletheia's eleven, eight consult a named
capability action before the effect happens; the other three — yield, exit, register check — carry
no object authority at all.

## Consequences

* **This is not a verdict, and the script says so in its own output.** A smaller surface is not a
  safer system. Surface is one input to risk. The others — maturity, adversarial review, exploit
  economics, defense in depth, and the plain fact that Linux has been attacked by professionals for
  thirty years while Aletheia has never been attacked by anyone — are unmeasured here, and every
  one of them favours Linux.
* **The syscall ratio is real and it is also unfair.** Linux's 375 entry points exist because it
  runs containers, graphics stacks, io_uring, BPF, and thirty years of ABI promises. Aletheia has
  eleven because it does almost nothing yet. **Removing features is not a security technique.** The
  narrower claim, which is the one worth making: on the syscall path Aletheia has, authority is
  named in one table and checked before the effect, rather than being ambient and confined later by
  an opt-in mechanism. That is a design difference, and the boot-gated ring-3/EL0/U-mode suites are
  what prove it — not this script.
* **`capability-gated: 0` for Linux is not a criticism.** Linux's model is ambient authority bounded
  by uid and capability sets, with confinement added on top by seccomp, SELinux/AppArmor and
  namespaces. Counting zero in that column means "this kernel does not gate syscalls on per-object
  capabilities", which is true, intentional, and carries workloads Aletheia cannot run.
* **Named non-claims.** Not measured, at all: exploitability, defect density, CVE history, side
  channels, speculative execution, firmware, or any other operating system. Windows, macOS, the
  BSDs and every RTOS are absent from this table and no claim is made about them.
* **The `unsafe` percentage is a language fact, not a defect measurement.** 2.02% of Aletheia's
  privileged lines are inside `unsafe`; 100% of Linux's are memory-unsafe by language and none of
  it is delimited. That says where the auditor should look, not how many bugs are there.
