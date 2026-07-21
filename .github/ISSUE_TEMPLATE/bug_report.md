---
name: Bug report
about: Report something that behaves incorrectly (a failing invariant, a crash, a wrong result)
title: "bug: "
labels: bug
---

## What happened

A clear description of the incorrect behavior.

## Which part

- [ ] Hosted System-Core (`aletheia/`)
- [ ] Microkernel — aarch64 (`kernel/`)
- [ ] Microkernel — x86-64 (`kernel-x86_64/`)
- [ ] Microkernel — RISC-V (`kernel-riscv64/`)
- [ ] Component runtime / SDK (`component-sdk/`, `examples/`)
- [ ] Docs / other

## Steps to reproduce

Exact commands, e.g.:

```bash
cargo test --manifest-path aletheia/Cargo.toml
# or
./scripts/vm-e2e-riscv.sh
```

## Expected vs. actual

- **Expected:**
- **Actual:** (paste the relevant output / failing assertion / VM exit code)

## Is a capability invariant involved?

If the bug is a possible **capability bypass**, **ambient-authority leak**, or a way for untrusted
content/model output to gain authority, please **do not** file it publicly — see the security note in
[CONTRIBUTING.md](../../CONTRIBUTING.md).

## Environment

- OS / arch:
- Rust: `rustc --version`
- QEMU (if kernel-related): `qemu-system-<arch> --version`
