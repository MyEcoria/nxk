/* Rust (std) programs for nxk: static-PIE linked from 0; the kernel picks a random base (ASLR). Page-aligned groups: RX code, R rodata (kernel-relocated RELRO), RW data. Copied into the target JSON by tools/mkstd.py. */
ENTRY(_start)

PHDRS
{
    text    PT_LOAD FLAGS(5);   /* R X */
    rodata  PT_LOAD FLAGS(4);   /* R   */
    data    PT_LOAD FLAGS(6);   /* R W */
    dynamic PT_DYNAMIC FLAGS(4);
    tls     PT_TLS FLAGS(4);
}

SECTIONS
{
    . = 0;

    .text : {
        KEEP(*(.text._start))
        *(.text .text.*)
    } :text

    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) } :rodata
    .init_array : {
        PROVIDE_HIDDEN(__init_array_start = .);
        KEEP(*(SORT_BY_INIT_PRIORITY(.init_array.*) .init_array))
        PROVIDE_HIDDEN(__init_array_end = .);
    } :rodata
    .data.rel.ro : { *(.data.rel.ro .data.rel.ro.*) } :rodata
    .got : { *(.got) *(.got.plt) } :rodata
    .dynamic : { *(.dynamic) } :rodata :dynamic
    .rela.dyn : { *(.rela.dyn) *(.rela.*) } :rodata
    .dynsym : { *(.dynsym) } :rodata
    .dynstr : { *(.dynstr) } :rodata
    .hash : { *(.hash) } :rodata
    .gnu.hash : { *(.gnu.hash) } :rodata

    . = ALIGN(4096);
    .tdata : { *(.tdata .tdata.*) } :data :tls
    .tbss : { *(.tbss .tbss.*) } :data :tls
    .data : { *(.data .data.*) } :data
    .bss : ALIGN(16) {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.comment) *(.note.*) *(.eh_frame) *(.eh_frame_hdr) *(.interp)
        *(.fini_array .fini_array.*) *(.gcc_except_table .gcc_except_table.*)
    }
}
