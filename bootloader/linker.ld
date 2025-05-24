OUTPUT_ARCH(riscv)

ENTRY(_start)

MEMORY {
    DRAM : ORIGIN = 0x80000000, LENGTH = 2M
}

SECTIONS {
    .text : {
        *(.text.entry)
        *(.text .text.*)
    } > DRAM

    .rodata : {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    } > DRAM

    .data : {
        *(.data .data.*)
        *(.sdata .sdata.*)
    } > DRAM

    .bss (NOLOAD) : {
        *(.bss.uninit)
        . = ALIGN(8);
        sbss = .;
        *(.bss .bss.*)
        *(.sbss .sbss.*)
        . = ALIGN(8);
        ebss = .;
    } > DRAM

    /DISCARD/ : {
        *(.eh_frame)
    }
}