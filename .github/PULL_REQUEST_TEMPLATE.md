<!-- Thanks for contributing to Aletheia. Keep the change focused; explain why, not just what. -->

## What this changes

<!-- A short description of the change and its motivation. -->

## Which invariant does it protect or advance?

<!-- Tie it to the model: a capability invariant, the intent→action pipeline, a VM boot gate,
     an experience-layer capability, etc. -->

## How I verified it (ADR-010: no blind code)

- [ ] `cargo test --manifest-path aletheia/Cargo.toml` — green
- [ ] `cargo clippy --manifest-path aletheia/Cargo.toml --all-targets -- -D warnings` — clean
- [ ] Kernel change? VM boot gate(s) green: `./scripts/vm-e2e.sh` / `./scripts/vm-e2e-riscv.sh` / `kernel-x86_64/scripts/smoke-test.sh`
- [ ] Component/SDK change? example fixture rebuilt (`scripts/build-example-component.sh`) and tests green
- [ ] Kept shared and unrelated files byte-identical

## Notes for reviewers

<!-- Anything non-obvious, trade-offs, or follow-ups. -->
