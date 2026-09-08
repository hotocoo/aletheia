# ADR-090 — Release artifacts are reproducible

**Status:** Accepted

## Context

Aletheia described its VMware images as deterministic, but `qemu-img convert` generated a fresh VMDK
`CID` on every conversion. ZIP entries also inherited filesystem timestamps. Two builds from the same
checkout could therefore differ despite both booting successfully.

## Decision

The release packager normalizes each generated monolithicSparse VMDK `CID` to a SHA-256-derived value
from its exact raw image payload. ZIP creation uses a repository-owned Python writer with sorted entries,
fixed timestamps, normalized permissions, and explicit compression settings.

`scripts/reproducible-release.sh` builds the same release twice and requires byte-identical ZIP artifacts
and matching ZIP digest metadata. It runs as a CI gate. The normal release script continues to boot the
packaged VMDKs before publication; reproducibility does not replace boot verification.

## Consequences

- Same source, pinned Rust toolchain, dependencies, build configuration and packaging environment produce
  byte-identical VMware release packages.
- Generated VMDK identity is stable per image payload rather than random.
- ZIP metadata no longer records host filesystem timestamps or permissions.
- Reproducibility is tested instead of inferred from deterministic image-builder code.
- Different external packaging-tool versions remain outside this claim; CI uses a fixed runner image and
  the complete artifact is measured by the gate.
