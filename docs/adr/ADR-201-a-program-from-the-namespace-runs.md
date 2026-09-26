# ADR-201 — A program from the namespace runs

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-USER-001 (new)
**Builds on:** ADR-199 (the advised scheduler at the console), ADR-081 (the memory boundary).

## Context

Every user-mode task this kernel had ever run was a stub assembled into the kernel image. Nothing
in the namespace could be executed; the hosted WASM components (ADR-014/065) run in `aletheiad`, not
in ring 0. A kernel that cannot load a program is not yet an operating system.

## Decision

* `kernel_core::elf::judge` decides, before a byte is mapped, whether bytes are a program THIS CPU
  can place: ELF64, little-endian, version 1, `ET_EXEC`, `e_machine` equal to the booting CPU
  (another CPU's image is refused with the `e_machine` it carries), a well-formed program-header
  table inside the file, exactly one `PT_LOAD` segment that is readable and executable and never
  writable, at the target's user code address, offset and address congruent modulo the alignment,
  at most one page, its file bytes inside the file, the entry point inside them. Every read is
  bounds-checked; the judgement is total on any byte slice.
* `run NAME` (capability `system.schedule`) reads the object, judges it, and hands the placement to
  the target: one user-mode task in a fresh address space whose code page is the segment, admitted
  through the resident advisor (ADR-199's fence, verdict reported), dispatched by the priority
  scheduler for at most 64 slices, torn down afterwards. The exit status is what the program passed
  to `SYS_EXIT`.
* A new medium's namespace is seeded with `hello`, built by `elf::build` around code that sums
  1..=10 in user mode and exits with the sum. The seed happens only on the boot that formatted the
  medium (`persist::note_formatted`, set where the machine's medium is formatted: the persistence witness, platform custody and the console's own mount), so a removed `hello` never returns.
  The three encodings were checked against LLVM's assembler.
* The host `console_ops` table classifies `run` Destructive: it executes code, so the agent needs a
  human's approval.

## Proof

* Host: `kernel-core/tests/elf.rs` (acceptance on each CPU, each refusal by name, and a seeded
  60,000-image mutation campaign, `ELF_SEED`, registered in `scripts/property-campaign.sh`);
  `run_places_a_judged_program_and_refuses_everything_else_by_name` in `tests/shell.rs`.
* Live, `scripts/console-e2e.sh`, all three CPUs under the running desktop: `run hello` twice, each
  "exited with status 55 after 1 slice(s)" with the advisor's verdict for the admission; `run
  manifesto` refused ("not an ELF image"); `run nosuch` answered "no such object"; the free frame
  count before and after is identical.

## Non-claims

* No way to put a binary into the namespace from the console yet (`write` takes typed text); the
  runnable object is the seeded one, or one written to the medium by other means.
* A program that never exits, or that takes an exception other than a page fault, is not contained:
  with the ADR-199 fence the slice is cooperative, so a spinning program holds the CPU, and the
  targets' unexpected-trap paths stop the machine. Containment (a slice budget enforced by the
  timer, exceptions routed to the supervisor) is the next rung.
* One segment, one page, no data segment, no relocations or PIE, no argv, no output beyond the exit
  status; `SYS_FS_*` stay reserved.
* The shipped System-1 console checkpoint predates `run`; requests for it route to System 2.
