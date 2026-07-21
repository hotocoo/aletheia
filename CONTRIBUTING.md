# Contributing to Aletheia

Thanks for your interest in Aletheia — a from-scratch, AI-native operating system organized around
seven primitives (**Entity → Capability → Context → Intent → Action → Memory → Relationship**), where
intelligence is a native but *untrusted* collaborator and all authority is an explicit **capability**
(no ambient authority). Please read this guide before opening an issue or a pull request.

## The one rule that matters most: contract honesty (ADR-010)

**No blind code.** Everything that claims to work must be *shown* to work by something automated:

- Logic and subsystems → tests (`cargo test`).
- Kernel / bring-up work → it must **boot in a VM and re-prove the invariants**, with the VM's exit
  code as the machine-checkable verdict.

We do not merge hardware or kernel code that has never executed. If you cannot run it, mark it clearly
as a design document, not an implementation.

## Project layout

| Path | What it is |
|------|------------|
| `aletheia/` | Hosted System-Core reference (Rust, runs in userspace — no hardware needed) |
| `kernel/` | `no_std` microkernel — **aarch64** (QEMU `virt`, bootstrap/dev target) |
| `kernel-x86_64/` | `no_std` microkernel — **AMD64/x86-64** (UEFI/OVMF) |
| `kernel-riscv64/` | `no_std` microkernel — **RISC-V/RV64GC** (QEMU `virt` + OpenSBI, S-mode) |
| `kernel-core/` | Arch-independent contracts shared by the kernel crates (the `Hal` trait) |
| `component-sdk/` | Author capability-secure WASM components in Rust |
| `examples/` | Example components (compiled to `wasm32-unknown-unknown`) |
| `docs/` | PRD, SAD, and ADRs — the sources of truth. Read the relevant ADR before changing a subsystem. |

## Development workflow

1. **Discuss first for anything non-trivial.** Open an issue describing the problem and your intended
   approach before writing a large change.
2. **Branch** off `main`; keep changes focused.
3. **Write the test/gate first** where practical (the invariant you are protecting).
4. **Implement**, keeping to the existing style of the file you are editing.
5. **Green before you push** — run the checks below.
6. **Open a PR** using the template; explain *what invariant your change protects* and *how you
   verified it*.

## Local checks (must pass before a PR)

```bash
# Hosted System-Core: acceptance + conformance + property + security + component + SDK + search
cargo test --manifest-path aletheia/Cargo.toml
cargo clippy --manifest-path aletheia/Cargo.toml --all-targets -- -D warnings

# Microkernel VM boot gates (each builds, boots in QEMU, and asserts 11/11 invariants + a PASS exit)
./scripts/vm-e2e.sh            # aarch64
./scripts/vm-e2e-riscv.sh      # riscv64  (needs qemu-system-riscv64)
cd kernel-x86_64 && ./scripts/smoke-test.sh   # x86-64 under OVMF/UEFI (expects QEMU exit 33)
```

CI (GitHub Actions + GitLab) runs the hosted acceptance suite and all three VM boot gates on every
push and pull request; they are hard gates.

### Toolchain

- **Rust** (stable for the hosted crate and the x86-64 UEFI target; **nightly + `rust-src`** for the
  `build-std` bare-metal aarch64/riscv targets — pinned per crate via `rust-toolchain.toml`).
- **QEMU** system emulators: `qemu-system-aarch64`, `qemu-system-riscv64`, `qemu-system-x86_64`.
- For components: `rustup target add wasm32-unknown-unknown`.

## Coding standards

- Idiomatic, safe Rust. `unsafe` requires a `// SAFETY:` comment stating the invariant it upholds.
- Keep the **capability discipline**: every action authorizes through the capability engine;
  authorization happens **before** any state is read into context; untrusted content is **data, never
  instruction**. New surfaces must be fail-closed.
- Match the style of the file you edit. Do not reformat unrelated code in the same change.
- Keep shared files (e.g. the kernel spine shared across targets) byte-identical unless your change is
  specifically about them.

## Commit and PR conventions

- **Conventional commits**: `type(scope): summary` — e.g. `feat(ai): capability-gated search`,
  `fix(kernel): …`, `test(component): …`, `docs: …`, `refactor(kernel): …`, `chore: …`.
- Explain *why*, not just *what*, in the body. Note how you verified the change (which test/gate).
- One logical change per PR where possible.

## Reporting security issues

Do **not** open a public issue for a security vulnerability (e.g. a capability bypass, an ambient-
authority leak, or a way for an untrusted component/model output to gain authority). Instead, contact
the maintainer privately so it can be fixed before disclosure.

## Code of conduct

Be respectful and constructive. Assume good faith. Harassment of any kind is not tolerated.

## License

By contributing, you agree that your contributions are licensed under the project's [MIT License](LICENSE).
