#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

use alloc::boxed::Box;
use alloc::vec::Vec;
use riscv::asm::wfi;

extern crate alloc;

mod arch;
mod config;
#[macro_use]
mod console;
mod board;
mod entry;
mod lang_item;
mod memory;
mod process;
mod syscall;
mod timer;
mod trap;

#[unsafe(no_mangle)]
extern "C" fn kmain(_hart_id: usize, dtb_addr: usize) -> ! {
    board::init(dtb_addr);
    trap::init();
    timer::init();
    memory::init();
    process::init();

    println!("[HEAP TEST] Attempting to exhaust memory by repeated small allocations...");

    let mut allocations: Vec<Box<[u8; 1024]>> = Vec::new();

    // 每次分配1KB

    loop {
        // 在实际内核中，每次循环前最好有办法输出信息或检查某种退出条件

        // 以免无限循环卡死而不是因为OOM panic

        // 例如，可以加一个计数器，如果分配次数过多而没有panic，则测试可能存在问题

        if allocations.len() % 100 == 0 && allocations.len() > 0 {
            // 每100次打印一次

            println!(
                "[HEAP TEST] OOM progress: allocated {} KB so far ({} blocks)",
                allocations.len(),
                allocations.len()
            );
        }

        // 为了避免编译器优化掉这个循环或者Box本身，可以做一些事

        // let b = Box::new([allocations.len() as u8; 1024]);

        // core::ptr::write_volatile(b.as_ptr() as *mut u8, allocations.len() as u8); // 确保写入

        // allocations.push(b);

        // 上面的方法更安全，但为了简单，我们直接push

        allocations.push(Box::new([0u8; 1024]));

        // 如果内核有tick或延时函数，可以在这里短暂延时，方便观察
    }

    println!("[kernel] Interrupts enabled, Kernel is running...");

    loop {
        wfi();
    }
}
