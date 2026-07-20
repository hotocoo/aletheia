# ADR-011 — Linux/POSIX only as an optional sandboxed compatibility environment

**Status:** Accepted
**Context:** Legacy software matters, but Linux must not be Aletheia's foundation.
**Decision:** Linux/POSIX exists only as an optional, sandboxed, capability-confined guest (Phase P6), mediated by a Compatibility Broker. Legacy syscalls become capability-checked Actions; filesystem paths become entity projections; devices/network require explicit capabilities + approval + audit. Aletheia is fully functional with no compatibility environment present.
**Consequences:** See `Aletheia_Compatibility_Environment_Appendix.md`. Forbids building on the Linux kernel or using systemd/X11 as infrastructure.
