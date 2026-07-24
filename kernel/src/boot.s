// Aletheia microkernel boot entry (aarch64, QEMU virt).
// Parks secondary cores, sets the stack, zeroes BSS, calls kmain.
.section .text.boot
.global _start
_start:
    // Only core 0 continues; park the rest.
    mrs     x1, mpidr_el1
    and     x1, x1, #0xff
    cbnz    x1, 3f

    // sp = __stack_top
    adrp    x0, __stack_top
    add     x0, x0, :lo12:__stack_top
    mov     sp, x0

    // zero .bss
    adrp    x1, __bss_start
    add     x1, x1, :lo12:__bss_start
    adrp    x2, __bss_end
    add     x2, x2, :lo12:__bss_end
1:  cmp     x1, x2
    b.hs    2f
    str     xzr, [x1], #8
    b       1b

2:  bl      kmain
    // kmain never returns; if it does, fall through to park.
3:  wfe
    b       3b

// PSCI CPU_ON entry for a secondary core (REQ-SMP-002): firmware delivers us here with the MMU
// off, IRQs masked, and x0 = the context argument — which core 0 sets to this core's private
// stack top. BSS is already zeroed (core 0 did it before any CPU_ON).
.global _secondary_start
_secondary_start:
    mov     sp, x0
    bl      ksecondary
    // ksecondary never returns; park defensively.
4:  wfe
    b       4b
