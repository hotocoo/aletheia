# The invariant index (ALET-P3-003)

One page answering: what are ALL the architectural invariants this project claims, where is each
WRITTEN, and where is each PROVED? The contracts live in docs/INVARIANT-CONTRACTS.md — this index
is the map over them, so no family can be added, renamed or deleted without the map noticing.
`scripts/check-boundary-docs.sh` enforces set parity between the contract sections and these
rows, in both directions.

Proof surfaces, in descending strength:

* **host** — a property/exhaustive suite in a crate's `tests/` (runs on every `cargo test`);
* **boot-gate** — an invariant asserted inside the kernel during every VM boot, counted by the
  gate's marker map (ADR-061) in the named script(s);
* **conformance** — additionally pinned as cross-CPU behavior in `scripts/conformance.sh`;
  a behavior that must not vary by CPU target;
* **e2e** — a whole-machine scripted gate driving real input/output through the emulator.

| Family | Contract § | Requirement | What it pins | Proof |
|--------|------------|-------------|--------------|-------|
| INV-TLB | Cross-core TLB shootdown | REQ-SMP-004 | all-acked completion, ack never precedes invalidation, exactly-once, aborted wait is false, bogus targets ignored | host: kernel-core/tests/shootdown.rs; boot-gate: vm-e2e.sh / vm-e2e-x86.sh |
| INV-PRIO | Priority inheritance | REQ-IPC-009 | holder never weaker than waiters, transitive donation, ends at release, never above highest base, cycles terminate, unauthorized acquire changes nothing | host: kernel-core/tests/priosched.rs |
| INV-IPC-CANCEL | Message cancellation | REQ-IPC-006 | cancelled never delivered, terminal-once, exact removal, one trace event per message, capacity freed without lifting bound, deadline/cancel exclusive | host: kernel-core/tests/ipc.rs |
| INV-CAP-REVOKE | Revocation under concurrency | REQ-CAP-006 | revoke is permanent/idempotent/forgery-proof, cascades transitively, commit-body interleaving yields all-or-nothing, siblings undisturbed | host: kernel-core/tests/cap_concurrency.rs |
| INV-FAULT | Page-fault classification | REQ-FAULT-001 | reserved bit dominates, uninterpreted arch bits are fatal-by-default, default fault is from-kernel | host: kernel-core/tests/faultclass.rs (exhaustive); boot-gate: vm-e2e-x86.sh |
| INV-REENTRY | Shared trap state | REQ-FAULT-002 | handler re-entry detectable and fatal, second-CPU entry caught, refusals leave evidence | host: kernel-core/tests/reentry.rs; boot-gate: vm-e2e-x86.sh |
| INV-LAYOUT | Address-space layout | REQ-MM-007/008 | declared regions validate, guard pages on all targets, VA 0 dead, KASLR posture written | host: kernel-core/tests/vmaddr.rs + layout; boot-gate + conformance |
| INV-TASK | Task lifecycle | REQ-SCHED-002 | Finished terminal, Blocked never dispatched, at most one Running, rotation accounting exact, unknown ids change nothing | host: kernel-core/tests/sched.rs |
| INV-STORE-ERR | Storage error semantics | REQ-STOR-004 | error kinds distinguishable, device errors surfaced incl. flush, cause preserved, refusals are no-ops byte-for-byte | host: kernel-core/tests/storage.rs |
| INV-DEADVA | Addresses that must be dead | REQ-MM-007/008 | null page and guard ranges unmapped in EVERY space incl. derived ones | boot-gate: vm-e2e.sh / vm-e2e-x86.sh + conformance |
| INV-CAP-SCOPE | The authority lattice | REQ-CAP-007 | attenuation order sound/reflexive/transitive by exhaustion; delegation cannot amplify (pattern-reach defect class) | host: kernel-core/tests/capalg.rs; conformance spine invariants |
| INV-CAP-LIFE | Capability lifetime across reboot | REQ-CAP-008 | store re-checks attenuation per edge, cascade re-derived not replayed, clock-rewind/id-reuse refused, whole-image load only | host: kernel-core/tests/capstore.rs + capstore_fs.rs |
| INV-KEYMAP | What a scancode means | REQ-CON-003 | decode output alphabet exhaustive over all scancodes × modifiers; every emitted byte has an editor rule | host: kernel-core/tests/keymap.rs; boot-gate all three targets + conformance |
| INV-CONSOLE-EDIT | Console parses its input | REQ-CON-004 | CSI sequences parsed not filtered, parser never left armed, parameters bounded, editor bounded history/completion | host: kernel-core/tests/shell.rs; e2e: keyboard-e2e.sh, console-e2e.sh |
| INV-CONSOLE-CMD | What commands do to the namespace | REQ-CON-005 | touch never truncates, mv copy-then-remove, append atomic via replace, bad numeric args refused, reboot honest | host: kernel-core/tests/shell.rs; boot-gate console families + conformance |
| INV-PS2 | Bringing the controller up | REQ-CON-003 | ACPI-gated probing, self-test then config read-back, translation enabled, IRQ1 masked after suite, end-to-end keystroke | boot-gate: vm-e2e-x86.sh; e2e: keyboard-e2e.sh |
| INV-SOAK | Lifecycles under repetition | REQ-QUAL-007 | journal churn allocates nothing (metered), byte-exact verification, structural soundness per op, grant churn zero-copy, task generations, seed-determinism | boot-gate soak suites on all three VM gates |
| INV-ATREST | Encryption at rest lifecycle | REQ-STORE-002 | plaintext-SHA identity both halves, constructed nonces globally distinct across reopens, exhaustive single-bit-flip refusal, position-bound frames, named key lifecycle, legacy migration | host: aletheia/tests/encryption_at_rest.rs |

## Reading the map

A row whose proof names ONLY a boot-gate is hardware-shaped behavior that cannot be honestly
host-tested (device sequencing, controller bring-up); its strength comes from running on every
boot of every gate. A row naming a host property suite runs on every test invocation. Rows with
conformance coverage hold IDENTICALLY on aarch64, RISC-V and x86-64 — the same named behavior,
not merely similar ones.
