/* Programmes Rust (std) pour nxk : executables statiques independants de la
   position (static-PIE), lies a partir de 0. Le chargeur du noyau choisit une
   base aleatoire (ASLR) et applique les relocations R_AARCH64_RELATIVE.

   Chaque groupe de sections commence sur une page pour recevoir ses propres
   droits : code RX, constantes et donnees relogees R (RELRO complet : le noyau
   les ecrit par adresse physique avant le demarrage), donnees RW. Le noyau
   impose W^X et refuse deux segments dans une meme page.

   Ce script est recopie dans la cible aarch64-unknown-nxk.json par
   tools/mkstd.py (champ link-script). */
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
