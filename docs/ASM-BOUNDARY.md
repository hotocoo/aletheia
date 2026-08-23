# The assembly/Rust boundary (ALET-P3-001, ADR-069 wave)

Every place Rust hands control to the machine — inline `asm!`, `naked_asm!` or
`global_asm!` — is the part of a kernel a reader cannot reason about from the types alone.
This page is the CENTRALIZED inventory of those places: one row per file, what its assembly is
FOR, and how many sites it holds. `scripts/check-boundary-docs.sh` regenerates the inventory
from the tree and fails when this page drifts in either direction (a new undocumented site, or a
row describing code that no longer exists).

**Counting rule (stated because it is enforced):** a SITE is any line of a `.rs` file under
`kernel/src`, `kernel-core/src`, `kernel-x86_64/src` or `kernel-riscv64/src` whose first
non-whitespace character is not `//` and which contains the token `asm!`, `naked_asm!` or
`global_asm!`. Purely-prose comment lines are excluded; trailing comments on code lines count,
because they sit on code.

## Why each boundary exists

The sites fall into five families, and every family has the same shape of justification: there
is no instruction encoding for it in Rust, and the compiler cannot be allowed to reorder around
it. **Entry/exit trampolines** (`usermode.rs`, `trap.rs`, `main.rs`, `exit.rs`) move
between privilege levels and save/restore frames whose exact layout the rest of the kernel
depends on. **Address-space control** (`vm.rs`) writes root registers and issues the
shootdown/fence instructions (`tlbi`, `sfence.vma`, `invlpg`) whose ordering against memory
accesses IS the semantics. **Synchronization and CPU lifecycle** (`smp.rs`, `arch.rs`,
`conirq.rs`, `bench.rs`) start secondaries, raise inter-processor interrupts, idle the core
and read cycle counters. **Device access** (`virtio.rs`, `pci.rs`, `hal.rs`,
`shellio.rs`, `semihosting.rs`, `sbi.rs`) performs MMIO with fences, port I/O, SBI calls
and semihosting traps that have no portable Rust spelling. Nothing else uses assembly: policy,
data structures, drivers' logic and all protocol code are plain safe-or-reviewed Rust.

## Inventory

| File | Sites | What the assembly is for |
|------|-------|--------------------------|
| `kernel/src/arch.rs` | 3 | aarch64 system-register read/write, fence + wfe/sev, counter reads |
| `kernel/src/bench.rs` | 4 | cycle-counter and barrier instructions for the boot benchmark family |
| `kernel/src/conirq.rs` | 3 | GIC interrupt acknowledge/EOI sequencing for the console input path |
| `kernel/src/main.rs` | 2 | entry hand-off from the `_start` stub; boot stack/exception-level setup |
| `kernel/src/semihosting.rs` | 2 | ARM semihosting call sequence (HLT instruction) |
| `kernel/src/shellio.rs` | 2 | polled PL011 UART read/write for early console output |
| `kernel/src/smp.rs` | 6 | secondary-CPU startup trampoline, PSCI CPU_ON conduit, IPI dispatch |
| `kernel/src/usermode.rs` | 9 | EL0 transition eret frames, syscall entry/return, context switch |
| `kernel/src/virtio.rs` | 1 | MMIO notify write with device-memory fence |
| `kernel/src/vm.rs` | 13 | TTBR0 load, TLBI shootdown variants, DSB/ISB barriers, descriptor walks |
| `kernel-riscv64/src/arch.rs` | 1 | csrrw/csrr system-register access |
| `kernel-riscv64/src/conirq.rs` | 4 | PLIC claim/complete sequencing for console interrupts |
| `kernel-riscv64/src/exit.rs` | 1 | QEMU test-finish exit path |
| `kernel-riscv64/src/main.rs` | 1 | entry hand-off from the SBI-booted start stub |
| `kernel-riscv64/src/sbi.rs` | 2 | SBI ecall wrappers (console putchar, system reset) |
| `kernel-riscv64/src/shellio.rs` | 1 | polled UART read/write for early console output |
| `kernel-riscv64/src/smp.rs` | 6 | secondary hart start via SBI HSM, IPI via CLINT/SSIP |
| `kernel-riscv64/src/trap.rs` | 5 | stvec vector stubs, scause/seepage-free save/restore frames |
| `kernel-riscv64/src/usermode.rs` | 9 | U-mode transition sret frames, syscall trampolines |
| `kernel-riscv64/src/virtio.rs` | 1 | MMIO notify write with fence |
| `kernel-riscv64/src/vm.rs` | 6 | satp load, sfence.vma shootdowns, fence.i |
| `kernel-x86_64/src/hal.rs` | 2 | port-mapped I/O (in/out), cli/sti/hlt |
| `kernel-x86_64/src/pci.rs` | 2 | legacy PCI config-space ports 0xCF8/0xCFC |
| `kernel-x86_64/src/shellio.rs` | 1 | polled 16550 UART I/O ports |
| `kernel-x86_64/src/smp.rs` | 3 | INIT-SIPI startup sequence, LAPIC EOI, pause |
| `kernel-x86_64/src/usermode.rs` | 2 | syscall/sysret fast-path and iretq ring-3 frames |
| `kernel-x86_64/src/virtio.rs` | 1 | MMIO notify write |

## Rules of the boundary

1. Assembly exists only where an INSTRUCTION has no Rust spelling or reordering would change
   semantics. A patch adding an asm! site for something Rust can express is wrong on sight.
2. Every site carries a comment stating WHAT the sequence does and what ordering it guarantees;
   the frame layouts these stubs build are pinned by const-asserts and register round-trip
   suites (ADR-039) so assembly and Rust cannot disagree silently.
3. This page must match the tree — the checker enforces both directions. Adding a site without
   documenting it here fails CI, and so does deleting code while leaving its row behind.
