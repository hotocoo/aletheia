# ADR-025: Secure boot and the complete chain of trust

**Status:** Proposed (deferred — phased plan, no blind code) · **Date:** 2026-07-22

## Context

Runtime capability security (ADR-003) must be complemented by boot-time integrity: a verifiable chain
from firmware through kernel, services, applications, and AI models (gap-register Issue 7). Without
it, an attacker who replaces a component on disk defeats every runtime guarantee.

## Decision

Establish a measured, signed chain of trust; each stage verifies the next before transferring control:

```text
Firmware → Verified Bootloader → Verified Kernel → Verified Services → Verified Apps → Verified Models
```

**Phase 1 — signing + verification format.** Define the signature envelope (a detached signature over
a content hash — reusing the store's hashing, ADR-005) and a key hierarchy (root → stage keys). The
kernel verifies each service/component signature before launch; a component is already a
content-addressed entity (ADR-014), so signature verification slots in at `install`/`run`. This layer
is **hosted-testable now**: sign a component fixture, tamper one byte, assert launch is refused.

**Phase 2 — platform root of trust.** UEFI Secure Boot (x86-64) / measured boot into a TPM PCR
(where present) / RISC-V equivalent, behind the `Hal`. Measured boot records each stage's hash; remote
attestation optional.

**Phase 3 — rollback + provenance.** Anti-downgrade policy (a monotonic version counter in secure
storage refuses older signed components); model provenance + integrity verified before execution
(ties to ADR-022 Phase 1); recovery path if verification fails (boot last-known-good, ADR-026).

## Consequences

- Unsigned or tampered boot components cannot execute under secure policy.
- Rollback protection is defined + testable; component and model provenance are verifiable.
- Phase 1 (signature verification at the component boundary) is hosted-testable and is the honest
  first slice; TPM/UEFI phases are hardware-bound. Stays `deferred` (`REQ-BOOT-001`) until Phase 1
  lands with a test.
