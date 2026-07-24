# Aletheia microkernel boot entry (RISC-V RV64, QEMU virt, S-mode via OpenSBI handoff).
# OpenSBI enters exactly ONE hart here (the boot hart — its lottery may pick any hartid; the rest
# wait in HSM STOPPED until sbi::hart_start). The first arrival claims boot atomically (belt and
# braces against non-HSM firmware entering several harts), records its hartid for Rust
# (BOOT_HART, in .data so BSS zeroing cannot clobber it), sets the stack, zeroes BSS, calls kmain.
# gp is intentionally left unset (0) so `la` stays PC-relative.
.section .text.boot
.global _start
_start:
    # First hart to arrive claims boot; any other arrival parks (it will be hart_start'ed later).
    # (.option arch: the global_asm assembler does not inherit the target's A extension.)
    la      t0, __boot_claim
    li      t1, 1
.option push
.option arch, +a
    amoswap.w.aq t2, t1, (t0)
.option pop
    bnez    t2, 3f

    # Record the boot hartid (SBI passes it in a0) BEFORE touching BSS.
    la      t0, BOOT_HART
    sd      a0, 0(t0)

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

# SBI HSM hart_start entry for a secondary hart (REQ-SMP-002): firmware delivers us here in
# S-mode with satp=0 (MMU off), SIE masked, a0 = hartid, a1 = opaque — which the boot hart sets
# to this hart's private stack top. BSS is already zeroed (boot hart did it before any hart_start).
.global _secondary_start
_secondary_start:
    mv      sp, a1
    call    ksecondary
    # ksecondary never returns; park defensively.
4:  wfi
    j       4b

.section .data
.balign 8
__boot_claim: .word 0
.global BOOT_HART
BOOT_HART: .dword 0xFFFFFFFFFFFFFFFF
