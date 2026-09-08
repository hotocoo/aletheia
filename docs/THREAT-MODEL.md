# Aletheia Security Threat Model

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-029, ALET-P2-030, ALET-P2-031
· **Builds on:** ADR-003, ADR-016, ADR-043, ADR-059, ADR-065, ADR-071, ADR-081

This is the maintained security-boundary inventory for the current architecture. Each boundary names
its crossing data, authoritative admission, unauthorized-effect failure, and availability/DoS failure.
Residual threats remain in the gap register until enforcement is actually delivered.

## 1. Adversary and trust assumptions

The adversary may supply model output, entity/component content, malformed device/network data,
forged/replayed/expired/revoked capability handles, hostile resource demand, and concurrent requests
that race authorization, revocation, approval, or ownership. The local model and entity/component
content are untrusted. Emulator evidence is not hardware evidence.

## 2. Security-boundary inventory

| ID | Boundary | Untrusted input | Authoritative admission | Security failure if bypassed | Availability failure if abused |
|---|---|---|---|---|---|
| B-01 | Model/content → validation | model output, entity content | structured intent validation | unauthorized effect | inference/validation CPU exhaustion |
| B-02 | Validation → capability engine | action, target, tokens | `CapEngine::evaluate` | capability bypass/amplification | authorization work exhaustion |
| B-03 | Capability → policy/approval | authorized action, risk | `PolicyEngine`, approval state | unauthorized destructive effect | approval queue exhaustion |
| B-04 | Service/API → System Core | subject, capability, command/query | service authorization | ambient authority / cross-subject access | request/stream exhaustion |
| B-05 | Component → host ABI | WASM calls, dependency requests | sandbox + host capability checks | sandbox escape / privilege gain | fuel/memory/table/stack/deadline exhaustion |
| B-06 | Kernel mapping API → MMU | VA/PA, permissions, roots | address/permission/ownership validation | memory isolation failure | mapping/reclamation exhaustion |
| B-07 | Kernel → DMA device | descriptor addresses, buffers | DMA registry + IOMMU/device window | device memory corruption | descriptor/buffer exhaustion |
| B-08 | Firmware → custody | firmware root/config | delivery parser + custody witness | forged/rolled-back authority | boot/custody resource exhaustion |
| B-09 | Device/network wire → parser | packets, options, lengths, checksums | bounded parser + protocol validation | parser confusion / unauthorized state | packet/poll exhaustion |
| B-10 | Input device → compositor | keycodes, axes, buttons | device classification + input session | cross-owner input injection | event queue flooding |
| B-11 | Compositor → framebuffer/GPU | surfaces, placement, pages | owner token + scanout bounds + DMA | cross-surface write | compose/frame exhaustion |
| B-12 | Allocator → resident services | free-frame/pressure readings | allocator-owned admission boundary | advisor bypass of memory authority | memory exhaustion |
| B-13 | Persistent bytes → state | store/capability images, journal | checksum/authentication + structural validation | forged/stale authority | oversized/corrupt-state processing |

**Rule:** a boundary is not closed merely because an upstream layer validates the same input. The
layer owning the effect must enforce its own authority invariant.

## 3. Security versus denial of service

**Unauthorized access/effect** means an actor obtains an effect it is not authorized to obtain:
capability bypass, privilege amplification, cross-owner observation, forged/replayed authority, or
execution outside a declared sandbox. The question is **who may cause what**.

**Denial of service (DoS)** means a bounded resource is consumed so another actor cannot make
progress: memory, queue slots, CPU budget, storage, I/O service, approval records, or descriptors.
DoS can occur while every authorization check is correct, and preventing DoS must not weaken
authorization.

Therefore: resource refusal is not authorization evidence; authorization denial must not become an
unbounded retry loop; bounded resources need explicit refusal/accounting/recovery; security tests
assert unauthorized effects and state preservation separately from exhaustion behavior.

## 4. Maintained evidence map

* B-01/B-02: `aletheia/src/intent_action.rs`, `aletheia/src/capabilities.rs`, `aletheia/tests/intent_confusion.rs`;
* B-03: `aletheia/src/policy.rs`;
* B-04: `aletheia/src/service.rs`, `aletheia/src/transport.rs`;
* B-05: `docs/adr/ADR-065-a-sandbox-bounded-in-every-dimension.md`;
* B-06: `kernel-core/src/vmaddr.rs`, `kernel-core/src/memattr.rs`, `kernel-core/src/frameown.rs`;
* B-07: `kernel-core/src/dma.rs`, `kernel-core/src/iommu.rs`, `kernel-core/src/vtd.rs`, `kernel-core/src/smmu.rs`;
* B-08: `kernel-core/src/bootroot.rs`, `kernel-core/src/capvault.rs`;
* B-09: `kernel-core/src/virtionet.rs`, `kernel-core/src/udpv4.rs`, `kernel-core/src/dhcp.rs`;
* B-10: `kernel-core/src/vinput.rs`;
* B-11: `kernel-core/src/compositor.rs`, `kernel-core/src/fbcon.rs`, `kernel-core/src/wm.rs`;
* B-12: `kernel-core/src/mlsched.rs`, `kernel-core/src/reclaim.rs`;
* B-13: `kernel-core/src/persist.rs`, `kernel-core/src/capstore.rs`, `kernel-core/src/compress.rs`.

`scripts/check-threat-model.sh` checks boundary-ID uniqueness, referenced source existence, and the
required threat-class sections. It is a consistency gate, not a claim of production completeness.

## 5. Residual threats

Interrupt-driven I/O, complete network protocols, live reclaim residency, hardware frequency control,
secure boot/update milestones, and hardware-level DMA isolation remain governed by their open/deferred
findings. They are not silently promoted to security-complete by this document.
