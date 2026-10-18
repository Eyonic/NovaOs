/* Nova's vDSO: one read-only page of data (nova_vvar) right below the object, which is a
   single PT_LOAD of headers, symbols and code, with Linux's LINUX_2.6 symbol versions. */
SECTIONS
{
    nova_vvar = . - 4096;
    . = SIZEOF_HEADERS;
    .hash           : { *(.hash) }          :text
    .gnu.hash       : { *(.gnu.hash) }
    .dynsym         : { *(.dynsym) }
    .dynstr         : { *(.dynstr) }
    .gnu.version    : { *(.gnu.version) }
    .gnu.version_d  : { *(.gnu.version_d) }
    .gnu.version_r  : { *(.gnu.version_r) }
    .dynamic        : { *(.dynamic) }       :text :dynamic
    .rodata         : { *(.rodata*) }       :text
    .text           : { *(.text*) }         :text
    /DISCARD/       : { *(.data*) *(.bss*) *(.got*) *(.plt*) *(.eh_frame*) *(.note*) *(.comment) }
}
PHDRS
{
    text    PT_LOAD    FLAGS(5) FILEHDR PHDRS;
    dynamic PT_DYNAMIC FLAGS(4);
}
VERSION
{
    LINUX_2.6 {
    global:
        clock_gettime; __vdso_clock_gettime;
        gettimeofday; __vdso_gettimeofday;
        time; __vdso_time;
        getcpu; __vdso_getcpu;
    local: *;
    };
}
