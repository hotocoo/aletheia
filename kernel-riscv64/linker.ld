/* QEMU 'virt' boots OpenSBI (M-mode) at 0x8000_0000, which hands off to the next stage — our
   `-kernel` ELF — at its link address in S-mode. The conventional S-mode payload base on 'virt'
   is 0x8020_0000 (OpenSBI reserves 0x8000_0000..0x8020_0000 for itself). We reserve a stack and a
   bump-allocator heap after BSS. RISC-V small-data sections (.sdata/.sbss/.srodata) are folded into
   their parent sections so the linker-defined heap/stack symbols stay correct. */
ENTRY(_start)

RAM_START = 0x80200000;
STACK_SIZE = 0x100000;   /* 1 MiB */
HEAP_SIZE  = 0x800000;   /* 8 MiB */

SECTIONS
{
    . = RAM_START;

    __text_start = .;
    .text : ALIGN(0x1000) {
        KEEP(*(.text.boot))
        *(.text*)
    }

    __text_end = .;
    .rodata : ALIGN(0x1000) {
        *(.rodata*)
        *(.srodata*)
    }

    __rodata_end = .;
    .data : ALIGN(0x1000) {
        *(.data*)
        *(.sdata*)
    }

    .bss : ALIGN(0x1000) {
        __bss_start = .;
        *(.bss*)
        *(.sbss*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }

    /* A GUARD PAGE below the kernel stack (GAPS4 ALET-P1-012, REQ-MM-007). The stack grows DOWN from
       __stack_top; without a guard, an overflow walks straight into .bss — silently corrupting whatever
       lives there and surfacing much later as impossible behavior. This page is reserved here and left
       UNMAPPED by `vm::build_identity`, so an overflow takes a translation fault at the first byte past
       the stack instead. It costs one page of address space and nothing at runtime. */
    . = ALIGN(0x1000);
    __stack_guard = .;
    . += 0x1000;

    __stack_bottom = .;
    . += STACK_SIZE;
    __stack_top = .;

    __heap_start = .;
    . += HEAP_SIZE;
    __heap_end = .;

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.riscv.attributes) }
}
