# ADR-008 — WASM/WASI capability-secure component model for portable apps

**Status:** Accepted
**Context:** Applications must provide capabilities and be composable and sandboxed, not be opaque data silos with ambient authority.
**Decision:** Portable applications/components target WebAssembly/WASI with no ambient authority — they receive only imported host capabilities mapped to Aletheia capabilities. Native components use System-Core APIs. Application data lives as entities. Each exposed operation is a registered tool {schema, required_caps, risk, executor, verifier}.
**Consequences:** Least-privilege, isolated, composable apps. WASM runtime (e.g. wasmtime) is P2; M1 ships the native tool registry + contract.
