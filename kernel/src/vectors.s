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
    b   default_exception          // 0x280 IRQ
    .balign 0x80
    b   default_exception          // 0x300 FIQ
    .balign 0x80
    b   default_exception          // 0x380 SError

    // ---- Lower EL, AArch64 (EL0 traps here) ----
    .balign 0x80
    b   el0_sync_entry             // 0x400 Synchronous  <-- EL0 svc / fault -> cap-gated boundary
    .balign 0x80
    b   default_exception          // 0x480 IRQ
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
