/* A read+execute PT_LOAD at the user code page and, when the program has globals, a read+write
   one at the data page the kernel maps above the stack (ADR-205, ADR-210). */
ENTRY(_start)
PHDRS { text PT_LOAD FILEHDR PHDRS FLAGS(5); data PT_LOAD FLAGS(6); }
SECTIONS {
  . = 0x40000000 + SIZEOF_HEADERS;
  .text : { *(.text._start) *(.text .text.*) *(.rodata .rodata.*) *(.got .got.*) } :text
  . = 0x40002000;
  .data : { *(.data .data.*) } :data
  .bss : { *(.bss .bss.*) *(COMMON) } :data
  /DISCARD/ : { *(.comment .eh_frame* .note*) }
}
