# ADR-014 — WASM components are capability-secure with no ambient authority; wasmi as the engine

**Status:** Accepted (P2 start — vertical slice delivered)

**Context:** Aletheia must run untrusted code — applications, tools, third-party agent bodies —
without granting any of them ambient authority (INV-011) and without letting untrusted content
become instruction (SEC-003, criteria 18–19). WebAssembly is the natural substrate: a sandboxed,
deterministic, language-agnostic instruction set with no built-in host access. The trap to avoid
is WASI's default surface: `wasmtime_wasi`'s standard linker hands a guest ambient filesystem,
clock, randomness, and environment — exactly the ambient authority the OS is defined to reject.
An engine must also be chosen. `wasmtime` is a production JIT but pulls in Cranelift and a larger
native build surface; `wasmi` is a pure-Rust interpreter, lighter, and `no_std`-capable.

**Decision:**
1. **No WASI.** A component reaches the OS only through an explicit host ABI (`aletheia.read`,
   `aletheia.write`, `aletheia.emit`), each of which authorizes through the **same**
   `CapEngine::evaluate` the deterministic pipeline uses, against the exact capability set the
   component was granted — nothing inherited from the launcher. Effects flow through the **same**
   `Store` and land in the one immutable event log. Untrusted linear-memory input is bounds-checked
   and copied out; a bad pointer returns an error, never traps the host.
2. **Two authority layers (application-as-capability).** Launching a component at all requires a
   `component.run` capability; the component then executes with **exactly** its `grant_caps`. An
   empty grant means it can do nothing.
3. **Execution is fuel-bounded.** A runaway component is trapped (out-of-fuel), never hangs the OS,
   and leaves no effects. This pre-stages the P2 stress/chaos gates.
4. **Engine: `wasmi`.** Pure-Rust, aligning with ADR-004 (no C toolchain in M1) and with eventually
   hosting components inside the `no_std` P4 kernel. For *proving invariants* (not maximizing
   throughput) an interpreter is the right first pick; a JIT engine can be revisited behind the same
   host ABI if throughput ever gates a milestone.

**Consequences:** The P2 acceptance test (`tests/component.rs`) proves the invariant that makes the
increment real: no capability → the component does nothing; an attenuated grant → it does exactly
that and no more; every allowed effect is in the audit log; a runaway is bounded; and launching is
itself gated. The host ABI is deliberately narrow (read returns content length, not bytes yet); a
richer return-buffer ABI, a component SDK, and multi-agent composition are follow-on P2 iterations
(their fuzzing/stress/chaos gates per PRD §22 come online with them). Choosing an interpreter trades
peak throughput for a smaller, auditable, `no_std`-friendly surface — the right trade while the
property under test is security, not speed. See PRD-003 §22, §45 (P2), and `src/component.rs`.
