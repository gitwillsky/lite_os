/* 系统服务的静态链接布局：各段按页对齐，段之间不共享页（ELF 装载器逐段映射独立 VMO）。
 * 装载基址由各服务的 build.rs 以 --defsym=SERVICE_BASE=... 给出。 */
ENTRY(_start)

PHDRS {
    text PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
}

SECTIONS {
    . = SERVICE_BASE;
    .text : { *(.text .text.*) } :text
    . = ALIGN(4K);
    .rodata : { *(.rodata .rodata.*) *(.srodata .srodata.*) *(.eh_frame_hdr) *(.eh_frame) } :rodata
    . = ALIGN(4K);
    .data : { *(.data .data.*) *(.sdata .sdata.*) *(.got .got.*) } :data
    .bss : { *(.bss .bss.*) *(.sbss .sbss.*) *(COMMON) } :data
    /DISCARD/ : { *(.comment) *(.note .note.*) }
}
