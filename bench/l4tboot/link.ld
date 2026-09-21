/* Loge sous 4 GiB (le vecteur de reset du coeur 0 est sur 32 bits), dans une
   zone reservee /no-map/ du DTB : Linux ne peut pas ecraser le moniteur PSCI. */
ENTRY(_start)
SECTIONS
{
    . = 0xFFE00000;
    .text : { KEEP(*(.text.boot)) *(.text .text.*) }
    .rodata : ALIGN(16) { *(.rodata .rodata.*) }
    .data : ALIGN(16) { *(.data .data.*) }
    .bss (NOLOAD) : ALIGN(16) { __bss_start = .; *(.bss .bss.*) *(COMMON); __bss_end = .; }
    . = ALIGN(4096);
    __stacks = .;
    . += 4 * 0x4000;
    __stack_top = .;
    /DISCARD/ : { *(.comment) *(.note.*) *(.eh_frame*) }
}
