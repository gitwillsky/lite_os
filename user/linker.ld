
OUTPUT_ARCH(riscv)
ENTRY(_start)

BASE_ADDRESS = 0x10000;

PHDRS
{
    text PT_LOAD FILEHDR PHDRS FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
    stack PT_GNU_STACK FLAGS(6);
}

SECTIONS
{
    . = BASE_ADDRESS + SIZEOF_HEADERS;
    .text : {
        *(.text.entry)
        *(.text .text.*)
    } :text
    . = ALIGN(4K);
    .rodata : {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    } :rodata
    . = ALIGN(4K);
    .data : {
        PROVIDE(__global_pointer$ = . + 0x800);
        *(.data .data.*)
        *(.sdata .sdata.*)
    } :data
    .bss : {
        *(.bss .bss.*)
        *(.sbss .sbss.*)
    } :data
    .stack (NOLOAD) : { } :stack
    /DISCARD/ : {
        *(.eh_frame)
        *(.debug*)
    }
}
