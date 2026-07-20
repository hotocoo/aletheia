/* QEMU 'virt' loads a -kernel ELF at its physical link address and jumps to the ELF
   entry (_start). RAM on 'virt' begins at 0x4000_0000; the -kernel load base is
   0x4008_0000. We reserve a stack and a bump-allocator heap after BSS. */
ENTRY(_start)

RAM_START = 0x40080000;
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
    }

    .data : ALIGN(0x1000) {
        *(.data*)
    }

    .bss : ALIGN(0x1000) {
        __bss_start = .;
        *(.bss*)
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

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}
