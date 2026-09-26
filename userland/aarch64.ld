/* One read+execute PT_LOAD at the user code page, headers included (ADR-205). */
ENTRY(_start)
PHDRS { text PT_LOAD FILEHDR PHDRS FLAGS(5); }
SECTIONS {
  . = 0x50000000 + SIZEOF_HEADERS;
  .text : { *(.text._start) *(.text .text.*) *(.rodata .rodata.*) } :text
  /DISCARD/ : { *(.data .data.* .bss .bss.* .comment .eh_frame* .note* .got*) }
}
