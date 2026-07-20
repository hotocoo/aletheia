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
