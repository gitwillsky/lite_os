OUTPUT_ARCH(aarch64)
ENTRY(_start)

BASE_ADDRESS = 0x40080000;
DIRECT_MAP_BASE = 0xffffffc000000000;
KERNEL_STACK_SIZE = 128K;

SECTIONS {
    . = BASE_ADDRESS;
    .boot : {
        KEEP(*(.text.entry))
    }
    kernel_text_phys = ALIGN(., 4K);

    . = DIRECT_MAP_BASE + kernel_text_phys;
    skernel = .;
    stext = .;
    .text : AT(ADDR(.text) - DIRECT_MAP_BASE) {
        . = ALIGN(4K);
        strampoline = .;
        KEEP(*(.text.trampoline))
        . = ALIGN(4K);
        *(.text .text.*)
    }
    . = ALIGN(4K);
    etext = .;

    srodata = .;
    .rodata : AT(ADDR(.rodata) - DIRECT_MAP_BASE) { *(.rodata .rodata.*) }
    . = ALIGN(4K);
    erodata = .;

    sdata = .;
    .data : AT(ADDR(.data) - DIRECT_MAP_BASE) { *(.data .data.*) }
    . = ALIGN(4K);
    edata = .;

    .bss : AT(ADDR(.bss) - DIRECT_MAP_BASE) {
        sbss = .;
        *(.bss .bss.*)
        *(COMMON)
    }
    . = ALIGN(4K);
    ebss = .;

    . = ALIGN(4K);
    boot_stack_bottom = .;
    . += 4K;
    boot_stack_mapped_bottom = .;
    . += KERNEL_STACK_SIZE;
    boot_stack_top = .;
    ekernel = .;
    kernel_image_end_phys = ekernel - DIRECT_MAP_BASE;

    /DISCARD/ : { *(.comment*) *(.eh_frame*) *(.gcc_except_table*) *(.note*) }
}
