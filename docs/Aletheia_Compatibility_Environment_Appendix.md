# Aletheia
## Compatibility Environment Appendix

**Document ID:** ALETHEIA-COMPAT-001
**Version:** 1.0.0
**Status:** Architecture Definition
**Related:** ALETHEIA-PRD-002 (§32), ALETHEIA-SAD-002
**Replaces:** the retired Linux Platform & OS Architecture Addendum (`*_v1_superseded.md`), which wrongly treated Linux as Aletheia's foundation.

---

# 1. Purpose

The v1 Linux addendum assumed Aletheia was built *on* Linux. That premise is void (PRD-002). This appendix defines the *only* legitimate role for Linux/POSIX in Aletheia: an **optional, sandboxed, capability-confined compatibility environment** for running legacy software. Aletheia MUST be a complete operating system with no compatibility environment present.

---

# 2. Principles

- **COMPAT-001 Optional and additive.** Aletheia's core value never depends on the compatibility environment. It is a feature, not a foundation. (PRD COMPAT-003)
- **COMPAT-002 Sandboxed and confined.** A compatibility guest runs isolated and holds only explicitly granted Aletheia capabilities. It has no ambient authority and cannot reach the semantic store, devices, network, or other entities except through capability-gated projections.
- **COMPAT-003 Filesystem is a projection, never truth.** Legacy POSIX filesystem calls are served by a path→entity projection over the semantic store (SAD §6, ST-009). The projection is read/write but never authoritative; every access is capability-checked; it cannot bypass the capability engine.
- **COMPAT-004 No privilege leakage.** Nothing inside the guest can obtain an Aletheia capability it was not granted, escalate to ambient authority, or influence the host System Core except through the same capability-gated IPC every client uses.

---

# 3. Architecture

```text
┌──────────────────────────────────────────────┐
│ Aletheia System Core (authority)             │
│   capability engine · semantic store · IPC   │
└───────────────┬──────────────────────────────┘
                │  capability-gated projections only
┌───────────────▼──────────────────────────────┐
│ Compatibility Broker (a capability-controlled │
│ System-Core service; translates guest syscalls│
│ into capability-checked Actions/projections)  │
└───────────────┬──────────────────────────────┘
                │  confined channel
┌───────────────▼──────────────────────────────┐
│ Sandboxed Legacy Guest (Linux/POSIX personality)│
│   legacy apps; sees a filesystem view + limited │
│   devices, all mediated and capability-scoped    │
└──────────────────────────────────────────────┘
```

The guest may be realized (in the P6 phase) via a user-mode POSIX personality or a virtualized/containerized Linux, whichever better preserves confinement. Either way the **broker** is the trust boundary: legacy syscalls become capability-checked Aletheia Actions, filesystem paths become entity projections, and device/network access requires explicit capabilities + approval + audit exactly as for native actors.

---

# 4. What This Appendix Does NOT Authorize

- Building the Aletheia kernel on the Linux kernel.
- Using systemd, X11/Wayland, or a Linux desktop compositor as Aletheia infrastructure.
- Granting legacy guests ambient authority or unmediated device/filesystem access.
- Treating the filesystem projection as the system of record.

The surviving idea from v1 is narrow and clear: **Linux can be a guest inside Aletheia; it can never be the ground Aletheia stands on.**

---

# 5. Phase

The compatibility environment is Phase P6 (PRD §41), after the microkernel (P4) and hardware/graphics/scheduler (P5). It has its own acceptance criteria centered on confinement: a legacy guest can perform no action for which it holds no capability, cannot read entities outside its granted scope, and cannot escape the broker.
