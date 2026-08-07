// EL1 exception vector table (aarch64). 16 entries × 0x80 bytes, table 2 KiB-aligned.
// Only the "Current EL with SP_ELx / Synchronous" slot (offset 0x200) is a fast path: an
// `svc` from EL1 lands there and returns immediately via `eret`. This is the irreducible
// hardware cost of one privilege-boundary round-trip — the syscall floor the benchmark
// measures. Every other vector is a fatal catch-all (default_exception -> exit 102).
.section .text
.balign 0x800
.global exc_vectors
exc_vectors:
    // ---- Current EL with SP_EL0 ----
    .balign 0x80
    b   default_exception          // 0x000 Synchronous
    .balign 0x80
    b   default_exception          // 0x080 IRQ
    .balign 0x80
    b   default_exception          // 0x100 FIQ
    .balign 0x80
    b   default_exception          // 0x180 SError

    // ---- Current EL with SP_ELx (kernel runs here) ----
    .balign 0x80
    eret                           // 0x200 Synchronous  <-- svc fast path
    .balign 0x80
    b   el1_irq_entry              // 0x280 IRQ          <-- console UART -> input ring (ADR-045)
    .balign 0x80
    b   default_exception          // 0x300 FIQ
    .balign 0x80
    b   default_exception          // 0x380 SError

    // ---- Lower EL, AArch64 (EL0 traps here) ----
    .balign 0x80
    b   el0_sync_entry             // 0x400 Synchronous  <-- EL0 svc / fault -> cap-gated boundary
    .balign 0x80
    b   el0_irq_entry              // 0x480 IRQ          <-- timer IRQ -> preemptive scheduler
    .balign 0x80
    b   default_exception          // 0x500 FIQ
    .balign 0x80
    b   default_exception          // 0x580 SError

    // ---- Lower EL, AArch32 ----
    .balign 0x80
    b   default_exception          // 0x600 Synchronous
    .balign 0x80
    b   default_exception          // 0x680 IRQ
    .balign 0x80
    b   default_exception          // 0x700 FIQ
    .balign 0x80
    b   default_exception          // 0x780 SError

// EL1 IRQ entry (vector 0x280). An interrupt taken while the KERNEL is running — until the
// interrupt-driven console there was no such thing here, and this slot was a fatal catch-all.
//
// Saves every caller-saved register the AAPCS64 lets a Rust function clobber (x0-x18, x29, x30);
// callee-saved ones the handler is obliged to preserve itself. No FP state: these kernels build for
// a softfloat target. IRQs stay masked for the whole handler (the CPU masks them on entry and we do
// not clear DAIF.I), so it cannot be re-entered and the frame cannot nest.
.global el1_irq_entry
el1_irq_entry:
    sub sp, sp, #176
    stp x0,  x1,  [sp, #0]
    stp x2,  x3,  [sp, #16]
    stp x4,  x5,  [sp, #32]
    stp x6,  x7,  [sp, #48]
    stp x8,  x9,  [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x29, [sp, #144]
    str x30,      [sp, #160]
    bl  el1_irq
    ldp x0,  x1,  [sp, #0]
    ldp x2,  x3,  [sp, #16]
    ldp x4,  x5,  [sp, #32]
    ldp x6,  x7,  [sp, #48]
    ldp x8,  x9,  [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x29, [sp, #144]
    ldr x30,      [sp, #160]
    add sp, sp, #176
    eret
