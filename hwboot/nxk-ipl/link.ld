/* nxk-ipl in IRAM, hekate's layout (bdk/memory_map.h): the payload runs
 * at 0x40008000 (RCM drops it at 0x40010000, _start moves it), 128 KiB
 * at most; stack under 0x4003FF00, the relocator's slot. */
ENTRY(_start)

SECTIONS {
    . = 0x40008000;
    __ipl_start = .;
    .text : {
        KEEP(*(.text._start))
        *(.text .text.*)
    }
    .rodata : { *(.rodata .rodata.*) }
    .data : { *(.data .data.*) }
    . = ALIGN(16);
    __ipl_end = .;
    .bss (NOLOAD) : {
        __bss_start = .;
        *(.bss .bss.* COMMON)
        . = ALIGN(4);
        __bss_end = .;
    }
    ASSERT(__ipl_end - __ipl_start <= 0x20000, "nxk-ipl: over 128 KiB")
    ASSERT(__bss_end <= 0x40030000, "nxk-ipl: bss reaches the SDRAM params buffer")
    /DISCARD/ : { *(.ARM.exidx*) *(.ARM.extab*) }
}
