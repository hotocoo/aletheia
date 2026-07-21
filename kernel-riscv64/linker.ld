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

    .text : ALIGN(0x1000) {
        KEEP(*(.text.boot))
        *(.text*)
    }

    .rodata : ALIGN(0x1000) {
        *(.rodata*)
        *(.srodata*)
    }

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

    . = ALIGN(0x1000);
    __stack_bottom = .;
    . += STACK_SIZE;
    __stack_top = .;

    __heap_start = .;
    . += HEAP_SIZE;
    __heap_end = .;

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.riscv.attributes) }
}
