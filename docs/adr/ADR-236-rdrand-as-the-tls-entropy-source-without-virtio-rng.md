# ADR-236 — RDRAND as the TLS entropy source on machines without virtio-rng

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-SEC-TLS-011 (tightened)
**Builds on:** ADR-153 (TLS keys only from a named entropy source), ADR-235 (the console's network
on the shipped VM).

## Context

ADR-153 made the console refuse to build a TLS key unless an entropy device it proved supplied the
seed, and the only such device was virtio-rng. ADR-235 gave the VMware package a working network,
but VMware and real x86-64 machines have no virtio-rng, so `tls` (and so `go` to an HTTPS host)
still refused there by name.

## Decision

* `kernel-x86_64/src/rdrand.rs` implements `entropy::EntropySource` over the RDRAND instruction,
  present when CPUID leaf 1 sets ECX bit 30. Each 64-bit draw is retried at most ten times while
  the CPU reports failure (CF clear), per Intel's DRNG guidance; a generator that keeps failing is
  `EntropyRefusal::Device` by name, never waited on.
* `fetch_tls` keeps virtio-rng as the first source. Only when the machine has none does it ask the
  CPU; with neither, it refuses as before. `entropy::tls_seed` still rejects a seed whose bytes
  are all one value.
* **Trust, stated:** RDRAND is the CPU vendor's generator and cannot be audited from software. It
  is used only where the alternative is no TLS at all, and it is not mixed with anything else yet.

## Evidence (2026-10-08)

* `scripts/tls-e2e.sh` gains an x86-64 leg with no virtio-rng and `-cpu qemu64,+smep,+rdrand`
  (QEMU emulates RDRAND from the host's generator): a dead port is refused, a wrong pin is
  refused, and the right pin completes a TLS 1.3 conversation with OpenSSL (8 records in, 3 out,
  38 bytes back). The virtio-rng legs on all three CPUs pass unchanged.
* Boundary inventories record the one new `asm!` site and the one new `unsafe`.

## Not done

* aarch64's RNDR (FEAT_RNG) is not used: the QEMU CPU model the gates run (cortex-a72) lacks it.
* No mixing of several sources and no continuous health test beyond the all-equal check.
