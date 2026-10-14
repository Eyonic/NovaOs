/* Linker script for the Nova kernel (Limine boot protocol, x86-64 higher half). */
OUTPUT_FORMAT(elf64-x86-64)
ENTRY(kmain)

PHDRS
{
    limine_requests PT_LOAD;
    text            PT_LOAD;
    rodata          PT_LOAD;
    data            PT_LOAD;
}

SECTIONS
{
    /* Limine requires the kernel in the top 2 GiB of the address space. */
    . = 0xffffffff80000000;

    /* Limine scans this region for request structures. */
    .limine_requests : {
        KEEP(*(.limine_requests_start))
        KEEP(*(.limine_requests))
        KEEP(*(.limine_requests_end))
    } :limine_requests

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .text : {
        *(.text .text.*)
        /* Linux objects (FEATURES=linux-drivers, see kernel/linuxkpi/): init and exit code
         * stays resident, and the CPU-specific alternatives are kept unapplied (the
         * default instructions in .text are always correct). */
        *(.init.text .exit.text .altinstr_replacement)
    } :text

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .rodata : {
        *(.rodata .rodata.*)
        *(.init.rodata .init.setup __param .altinstructions)
        . = ALIGN(8);
        __ex_table_start = .;
        KEEP(*(__ex_table))
        __ex_table_end = .;
        /* Linux initcalls in level order; lx_emul/pci.c runs them (nova_lx_start). */
        . = ALIGN(8);
        __nova_initcall_start = .;
        KEEP(*(.initcallearly.init))
        KEEP(*(.initcall0.init)) KEEP(*(.initcall0s.init))
        KEEP(*(.initcall1.init)) KEEP(*(.initcall1s.init))
        KEEP(*(.initcall2.init)) KEEP(*(.initcall2s.init))
        KEEP(*(.initcall3.init)) KEEP(*(.initcall3s.init))
        KEEP(*(.initcall4.init)) KEEP(*(.initcall4s.init))
        KEEP(*(.initcall5.init)) KEEP(*(.initcall5s.init))
        KEEP(*(.initcallrootfs.init))
        KEEP(*(.initcall6.init)) KEEP(*(.initcall6s.init))
        KEEP(*(.initcall7.init)) KEEP(*(.initcall7s.init))
        __nova_initcall_end = .;
    } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .data : { *(.data .data.*) *(.init.data .exit.data) } :data
    .bss : { *(.bss .bss.*) *(COMMON) } :data

    /DISCARD/ : {
        *(.eh_frame*) *(.note .note.*) *(.comment)
        *(.modinfo) *(.export_symbol) *(.discard .discard.*)
    }
}
