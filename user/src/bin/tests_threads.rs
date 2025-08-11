#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;
use alloc::boxed::Box;

use user_lib::{thread_create, thread_join, thread_exit, exit};

// 通过堆分配线程栈，避免使用 static mut

#[unsafe(no_mangle)]
extern "C" fn thread_entry(arg: usize) -> ! {
    // 简单根据 arg 返回不同的 code
    let code = (arg as i32) & 0xFF;
    thread_exit(code)
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    test_info!("threads: 开始线程接口测试");

    let mut code0: i32 = -1;
    let mut code1: i32 = -1;

    // 使用堆分配并泄漏，避免 static mut 引用
    let stack0: &'static mut [u8] = Box::leak(alloc::vec![0u8; 4096].into_boxed_slice());
    let sp0 = stack0.as_mut_ptr() as usize + 4096;
    let stack1: &'static mut [u8] = Box::leak(alloc::vec![0u8; 4096].into_boxed_slice());
    let sp1 = stack1.as_mut_ptr() as usize + 4096;

    let tid0 = thread_create(thread_entry as usize, sp0, 7);
    test_assert!(tid0 > 0, "thread_create 失败: {}", tid0);
    let tid1 = thread_create(thread_entry as usize, sp1, 9);
    test_assert!(tid1 > 0, "thread_create 失败: {}", tid1);

    let r0 = thread_join(tid0 as usize, &mut code0);
    let r1 = thread_join(tid1 as usize, &mut code1);
    test_assert!(r0 == 0 && r1 == 0, "thread_join 失败: {}, {}", r0, r1);
    test_assert!(code0 == 7 && code1 == 9, "线程退出码异常: {} {}", code0, code1);

    test_info!("threads: 所有用例通过");
    exit(0);
    0
}


