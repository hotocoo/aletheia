# aletheia (M1 — Hosted System-Core Reference)

Rust reference implementation of the Aletheia System Core: an **AI-native operating system** organized
around seven primitives — Entity, Capability, Context, Intent, Action, Memory, Relationship — where
intelligence is native but **untrusted**, authority is always an explicit **capability** (no ambient
authority), and a deterministic pipeline executes and **verifies** everything. See `../docs/`.

## Quickstart

```bash
cargo test     # 18 tests: the 20 M1 acceptance criteria + adversarial security checks
cargo run      # aletheiad — hosted experience surface; runs the demo and prints explainable traces
```

## Layout (modules mirror the SAD crate boundaries; dependency direction points inward to `domain`)

| module | role |
|--------|------|
| `domain` | seven-primitive model: Entity, Relationship, Event, ids, errors |
| `crypto` | SHA-256 content addressing + ChaCha20-Poly1305 encryption at rest |
| `storage` | encrypted, content-addressed, versioned, durable semantic store |
| `capabilities` | unforgeable tokens, attenuated delegation, revocation, fail-closed evaluation |
| `worldmodel` | provenance-aware relationship traversal |
| `context` / `memory` | thin M1 types (ranking/distillation are post-M1) |
| `intelligence` | `ModelRuntime` port + deterministic fallback + local-model adapter |
| `tools` | operation registry (risk + required capability) |
| `intent_action` | Intent/Plan/Trace + parse/validate stages |
| `agents` | first-class, capability-bounded actors |
| `syscore` | composition + the deterministic intent→action pipeline + task runtime |
| `experience` | hosted surface: renders traces, world model, audit log |

## Invariants (enforced + tested)

Intelligence cannot directly execute. Every action passes a registered operation, capability
evaluation (fail closed), and verification. Destructive actions require approval. No ambient authority.
Untrusted content is data, not instruction. A failed/malformed interpretation cannot corrupt state.
The OS is fully functional with no resident model.
