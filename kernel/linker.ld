OUTPUT_ARCH(riscv)

/* 入口点为 _start 函数 */
ENTRY(_start)

/* 程序加载到 opensbi 指定的地址 */
BASE_ADDRESS = 0x80200000;

/* 定义内核栈大小 */
KERNEL_STACK_SIZE = 16K;

SECTIONS {
    . = BASE_ADDRESS;
    skernel = .;

    stext = .;
       .text : {
        *(.text.entry)
        . = ALIGN(4K);
        strampoline = .;
        *(.text.trampoline);
        . = ALIGN(4K);
        *(.text .text.*)
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
        sbss = .;
        *(.bss .bss.*)
        *(.sbss .sbss.*)
    }
    . = ALIGN(4K);
    ebss = .;

    /* 为启动栈分配空间 */
    . = ALIGN(16);
    boot_stack_bottom = .;
    . += KERNEL_STACK_SIZE;
    boot_stack_top = .;

    ekernel = .;

    /DISCARD/ : {
        /* 丢弃语言异常处理的栈展开信息 */
        *(.eh_frame)
    }
}