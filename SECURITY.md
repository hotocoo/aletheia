# Security Policy

Aletheia's entire premise is a security property: **all authority is an explicit, unforgeable
capability, and there is no ambient authority**. A security issue here is therefore usually a way to
*get authority you were never granted*, not just a crash.

## Supported versions

Aletheia is pre-1.0 and moves fast. Only the current `main` branch is supported; fixes land there.

## What counts as a vulnerability

Report privately (see below) if you find any of these:

- **Capability bypass** — performing an action without holding a capability that authorizes it, or a
  forged/replayed token being accepted as authority.
- **Privilege amplification** — a delegated capability that grants *more* than its parent, or
  revocation that fails to cascade.
- **Ambient authority** — any path that acts on the caller's behalf without an explicit capability
  (including a component reaching the OS outside the host ABI, or authority leaking between runs).
- **Untrusted-input escalation** — untrusted entity content or a model's output being treated as
  instruction/authority rather than as data (SEC-003), or a malformed/adversarial model output that
  causes an unauthorized effect.
- **Isolation escape** — a WASM component escaping its fuel/host-ABI boundary or affecting state it
  was not authorized to.
- Leaks of capability tokens or decrypted content through logs, the audit surface, or the IPC boundary.

## How to report

**Do not open a public issue for a security vulnerability.**

- Preferred: open a private advisory at
  <https://github.com/hotocoo/aletheia/security/advisories/new>.
- Please include: what invariant is broken, a minimal reproduction (a test, WAT/wasm component, or
  command), and the impact.

We aim to acknowledge a report within a few days and to fix confirmed issues on `main` before public
disclosure. Please give us reasonable time to remediate before disclosing.

## Scope notes

- Findings must be reproducible against `main` (a hosted `cargo test` case or a VM boot gate is ideal).
- The threat model treats the local model and all component/entity content as **untrusted**; reports
  that assume a trusted model or trusted content are working as designed, not vulnerabilities.
