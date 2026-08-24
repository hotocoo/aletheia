/* QEMU 'virt' loads a -kernel ELF at its physical link address and jumps to the ELF
   entry (_start). RAM on 'virt' begins at 0x4000_0000; the -kernel load base is
   0x4008_0000. We reserve a stack and a bump-allocator heap after BSS. */
ENTRY(_start)

RAM_START = 0x40080000;
STACK_SIZE = 0x100000;   /* 1 MiB */
HEAP_SIZE  = 0xC00000;   /* 12 MiB - custody residency grew with ADR-072: the vault\n                         keeps its keystore, image and witness state live on the bump heap */

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
    }

    __rodata_end = .;
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

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}
