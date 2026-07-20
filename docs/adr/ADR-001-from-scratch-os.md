# ADR-001 — Aletheia is a from-scratch OS, not a host-OS application

**Status:** Accepted (supersedes the v1 Linux-hosted-app direction)
**Context:** v1 defined Aletheia as an AI app on Linux, inheriting Process/File/Window primitives and ambient authority. The product owner corrected this: Aletheia is an operating system designed from first principles around intelligence, context, intent, memory, relationships, and capabilities.
**Decision:** Build Aletheia as an OS: minimal microkernel + System Core + experience layer. Do not build on the Linux kernel or inherit Unix/systemd/X11 assumptions. Re-evaluate every inherited abstraction (see ADR-002).
**Consequences:** v1 PRD/SAD/Linux-addendum retired to `*_v1_superseded.md`. Implementation follows PRD-002/SAD-002. Higher engineering cost; correct substrate for safe native intelligence. A hosted reference precedes metal (ADR-010).
