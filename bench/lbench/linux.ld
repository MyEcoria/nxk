/* Executable Linux statique (ET_EXEC), charge a l'adresse habituelle. */
ENTRY(_start)
SECTIONS
{
    . = 0x400000 + SIZEOF_HEADERS;
    .text : { KEEP(*(.text._start)) *(.text .text.*) }
    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) }
    . = ALIGN(4096);
    .data : { *(.data .data.*) }
    .bss : ALIGN(16) { *(.bss .bss.*) *(COMMON) }
    /DISCARD/ : { *(.comment) *(.note.*) *(.eh_frame*) }
}
