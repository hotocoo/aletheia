/* A read+execute PT_LOAD at the user code pages and, when the program has globals, a read+write
   one at the data page the kernel maps above the stack (ADR-205, ADR-210, ADR-211). The kernel
   reserves 16 code pages, then the stack page, then up to 16 data pages (ADR-214). */
ENTRY(_start)
PHDRS { text PT_LOAD FILEHDR PHDRS FLAGS(5); data PT_LOAD FLAGS(6); }
SECTIONS {
  . = 0x50000000 + SIZEOF_HEADERS;
  .text : { *(.text._start) *(.text .text.*) *(.rodata .rodata.*) *(.got .got.*) } :text
  . = 0x50011000;
  .data : { *(.data .data.*) } :data
  .bss : { *(.bss .bss.*) *(COMMON) } :data
  /DISCARD/ : { *(.comment .eh_frame* .note*) }
}
ASSERT(SIZEOF(.data) + SIZEOF(.bss) <= 16 * 4096, "a program's data and bss exceed the 16 data pages the kernel maps")
