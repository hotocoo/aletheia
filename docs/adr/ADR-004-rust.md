# ADR-004 — Rust-first: Rust is the primary language for kernel, core, and system

**Status:** Accepted (expanded per product-owner Rust-first mandate)

**Context:** An AI-native OS with a capability-secure substrate needs memory safety, systems-level
performance, and freedom from legacy Unix/Linux language assumptions. Historical compatibility with
C/Unix must not drive language choice.

**Decision:** Rust is the **primary** implementation language for:
- the microkernel
- the system core
- the security architecture and capability system
- system services
- the storage engine
- the actor/task runtime
- AI runtime orchestration
- the developer SDK

C, C++, and assembly MAY be used **only** behind explicitly defined, minimal, audited interfaces,
and only where genuinely required for:
- hardware initialization
- architecture-specific functionality
- vendor SDKs
- graphics infrastructure
- AI acceleration
- existing high-performance libraries

No language shall be selected merely for historical compatibility with Linux or Unix. Every non-Rust
interface MUST be minimal, explicitly bounded, and audited; `unsafe` Rust MUST likewise be minimized
and isolated. Portable applications/components target WASM/WASI (ADR-008), not native C ABIs.

**Consequences:**
- Strong memory-safety guarantees for the capability engine, store, and pipeline.
- FFI to C/C++/asm is an exception requiring justification against the list above, a minimal surface,
  and an audit note — not a default.
- The M1 hosted reference is 100% safe Rust (a cargo workspace of small crates; dependency direction
  points inward toward `domain`). No C toolchain dependency in M1: content addressing (`sha2`) and
  encryption at rest (`chacha20poly1305`) are pure-Rust crates; the store is a pure-Rust engine.
- Later hardware phases (P4/P5) introduce audited FFI only where the list above demands it
  (e.g. GPU/AI-accelerator vendor interfaces, arch-specific boot/init).
