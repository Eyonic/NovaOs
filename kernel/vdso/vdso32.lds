/* Nova's 32-bit vDSO (vdso32.c), laid out like vdso.lds: one read-only page of data (nova_vvar) right below the object, which is a
   single PT_LOAD of headers, symbols and code, with Linux's LINUX_2.6 symbol versions. */
OUTPUT_FORMAT("elf32-i386")
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
    /* i386 PIC addresses nova_vvar relative to the GOT, which stays (and stays empty). */
    .got            : { *(.got) *(.got.plt) } :text
    /DISCARD/       : { *(.data*) *(.bss*) *(.plt*) *(.eh_frame*) *(.note*) *(.comment) }
}
PHDRS
{
    text    PT_LOAD    FLAGS(5) FILEHDR PHDRS;
    dynamic PT_DYNAMIC FLAGS(4);
}
VERSION
{
    LINUX_2.5 {
    global:
        __kernel_vsyscall;
    local: *;
    };
    LINUX_2.6 {
    global:
        __vdso_clock_gettime; __vdso_clock_gettime64;
        __vdso_gettimeofday;
        __vdso_time;
        __vdso_getcpu;
    local: *;
    } LINUX_2.5;
}
