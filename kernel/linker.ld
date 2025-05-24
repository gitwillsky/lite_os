OUTPUT_ARCH(riscv)

/* 入口点为 _start 函数 */
ENTRY(_start)

/* 程序加载到 opensbi 指定的地址 */
BASE_ADDRESS = 0x80200000;

SECTIONS {
    . = BASE_ADDRESS;
    skernel = .;

    stext = .;
    .text : {
       *(.text.entry) /* 放置入口代码 */
       *(.text .text.*) /* 剩余代码段 */
    }
    . = ALIGN(4K);
    etext = .;

    srodata = .;
    .rodata : {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }
    . =ALIGN(4K);
    erodata = .;


    /* .data 段： 包含已初始化的全局变量和静态变量*/
    sdata = .;
    .data : {
        *(.data .data.*)
        *(.sdata .sdata.*)
    }
    . = ALIGN(4K);
    edata = .;

    /* .bss 段：包含未初始化的全局变量和静态变量, 不会在可执行文件中为其
        分配实际的空间（除了一些元数据）。启动代码（_start) 负责在运行时将这块内存区域
        清零
     */
    .bss : {
        *(.bss.stack)
        sbss = .;
        *(.bss .bss.*)
        *(.sbss .sbss.*)
    }
    . = ALIGN(4K);
    ebss = .;

    ekernel = .;

    /DISCARD/ : {
        *(.eh_frame) /* 丢弃语言异常处理的栈展开信息 */
    }
}