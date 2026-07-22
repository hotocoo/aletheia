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

**Phase 1 — signing + verification format (delivered, hosted).** A detached signature over a
component's content hash (reusing the store's hashing, ADR-005). A component is already a
content-addressed entity (ADR-014), so verification slots in at the launch boundary. Under an opt-in
secure policy (`SysCore::set_require_signed_components`, default off): `install_signed_component`
refuses an untrusted/tampered signature at install; `run_installed` verifies the stored signature over
the content hash and refuses an unsigned/invalid component fail-closed; and the ad-hoc raw-WASM
`run_component` path — which carries no provenance — is refused entirely under secure policy, so there
is no bypass. Proved hosted (`aletheia/tests/component_signing.rs`): sign a fixture → launches; tamper
/untrusted/unsigned/ad-hoc → refused. Implemented with symmetric HMAC-SHA256 (`crypto::hmac_sha256`,
built on the existing `sha2`).

**Asymmetric provenance (REQ-BOOT-002, delivered 2026-07-22, `aletheia/src/provenance.rs`).** The
symmetric HMAC store lets the verifier forge (it holds the secret). The asymmetric path fixes that:
`SigningIdentity` holds the PRIVATE key and signs; `AsymTrustStore` holds trusted **public keys only**
(no private material), so a compromised verifier cannot forge. Includes the **root→signing-key
hierarchy** — a trusted root endorses a component-signing key (`endorse`), and `verify_chain` accepts a
component only if a trusted root endorsed its signer AND the signer signed the component (delegation +
rotation without trusting every signer). Ed25519 (pure-Rust dalek; ADR-004). Fail-closed on empty
store / malformed key / malformed signature / tamper / unendorsed signer / untrusted-root endorsement.
Hosted-proved (3 tests, fixed-seed keypairs). This is the signing FORMAT + trust hierarchy; the
platform root of trust and anti-rollback below remain hardware-bound (REQ-BOOT-001).

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
