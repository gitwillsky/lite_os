#![no_std]
#![feature(linkage)]

use core::arch::global_asm;

pub mod syscall;
#[macro_use]
pub mod console;

mod lang_item;

pub use syscall::*;

global_asm!(
    r#"
    .section .text.entry
    .global _start
    .type _start, @function
_start:
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop
    mv a0, sp
    call __user_start
1:
    j 1b
    .size _start, . - _start
"#
);

/// @description 解析 Linux/riscv64 初始栈并进入用户 `main`。
///
/// @param initial_stack 指向 `argc, argv..., NULL, envp..., NULL, auxv...` 的 16-byte aligned 栈顶。
/// @return 调用 `exit_group` 终止进程，此函数不返回。
#[unsafe(no_mangle)]
extern "C" fn __user_start(initial_stack: *const usize) -> ! {
    // 1. kernel 保证初始栈可读且按 RV64 word 对齐；argc 后紧跟 argv 指针。
    // SAFETY: process-entry ABI guarantees a readable aligned initial stack containing argc,
    // argc argv pointers, a null terminator, then envp.
    let argc = unsafe { initial_stack.read() };
    let argv = unsafe { initial_stack.add(1) }.cast::<*const u8>();
    // 2. argv 的 argc 个元素后有一个 NULL，其后即 envp。
    let envp = unsafe { argv.add(argc + 1) };
    exit_group(main(argc, argv, envp))
}

#[linkage = "weak"]
#[unsafe(no_mangle)]
extern "C" fn main(_argc: usize, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    panic!("user program has no main entry")
}
