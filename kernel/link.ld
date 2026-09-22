/* nxk - image loaded in Tegra X1 DRAM (0x80000000).
   Sections aligned to 4 KiB for distinct page rights (RX / RO / RW-NX). */

ENTRY(_start)

KERNEL_BASE  = 0x80000000;
STACK_SIZE   = 0x4000;      /* 16 KiB per core: short kernel paths */
NR_CPUS      = 4;

SECTIONS
{
    . = KERNEL_BASE;
    __kernel_start = .;

    .text : ALIGN(4096) {
        __text_start = .;
        KEEP(*(.text.boot))
        *(.text .text.*)
        . = ALIGN(4096);
        __text_end = .;
    }

    .rodata : ALIGN(4096) {
        __rodata_start = .;
        *(.rodata .rodata.*)
        . = ALIGN(8);
        *(.got .got.plt)
        . = ALIGN(4096);
        __rodata_end = .;
    }

    .data : ALIGN(4096) {
        __data_start = .;
        *(.data .data.*)
        . = ALIGN(4096);
        __data_end = .;
    }

    .bss (NOLOAD) : ALIGN(4096) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    /* Kernel stacks: one per core, page-aligned. */
    .stacks (NOLOAD) : ALIGN(4096) {
        __stacks_start = .;
        . += STACK_SIZE * NR_CPUS;
        __stack_top = .;
    }

    . = ALIGN(4096);
    __kernel_end = .;

    /DISCARD/ : { *(.comment) *(.note.*) *(.eh_frame) *(.eh_frame_hdr) }
}
