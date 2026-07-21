# Aletheia microkernel boot entry (RISC-V RV64, QEMU virt, S-mode via OpenSBI handoff).
# OpenSBI enters here with a0 = hartid, a1 = DTB pointer. Parks secondary harts, sets the
# stack, zeroes BSS, calls kmain. gp is intentionally left unset (0) so `la` stays PC-relative.
.section .text.boot
.global _start
_start:
    # Only hart 0 continues; park the rest (SBI passes hartid in a0).
    bnez    a0, 3f

    # sp = __stack_top
    la      sp, __stack_top

    # zero .bss  (t0 = start, t1 = end)
    la      t0, __bss_start
    la      t1, __bss_end
1:  bgeu    t0, t1, 2f
    sd      zero, 0(t0)
    addi    t0, t0, 8
    j       1b

2:  call    kmain
    # kmain never returns; if it does, fall through to park.
3:  wfi
    j       3b
