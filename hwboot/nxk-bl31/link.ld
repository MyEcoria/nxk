/* nxk-bl31 : charge a 0xFFF00000 (TZDRAM 1 Mo du chemin L4T hekate sur
   Erista ; meme adresse sous l'emulateur, qui a 4 Gio de DRAM).
   .text/.rodata/.data petits ; une pile de 16 Kio par coeur. */
ENTRY(_start)

BL31_BASE  = 0xFFF00000;
STACK_SIZE = 0x4000;
NR_CPUS    = 4;

SECTIONS
{
    . = BL31_BASE;
    __image_start = .;

    .text : ALIGN(4096) {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }
    .rodata : ALIGN(16) { *(.rodata .rodata.*) }
    .data : ALIGN(16) { *(.data .data.*) }
    .bss (NOLOAD) : ALIGN(16) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    }
    . = ALIGN(4096);
    __stacks = .;
    . += STACK_SIZE * NR_CPUS;
    __stack_top = .;
    __image_end = .;
    /DISCARD/ : { *(.comment) *(.note.*) *(.eh_frame*) }
}
