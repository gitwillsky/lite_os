pub mod context;

use core::{
    arch::{asm, global_asm},
    panic,
};

pub use context::TrapContext;
use riscv::{
    ExceptionNumber, InterruptNumber,
    interrupt::{Exception, Interrupt, Trap},
    register::{
        self, scause, sepc, stval,
        stvec::{self, TrapMode},
    },
};

use crate::{
    memory::{TRAMPOLINE, TRAP_CONTEXT},
    syscall,
    task::{
        current_trap_cx, exit_current_and_run_next, suspend_current_and_run_next, task_manager::current_user_token
    },
    timer,
};

global_asm!(include_str!("trap.S"));

pub fn init() {
    set_kernel_trap_entry();

    unsafe {
        // 启用时钟中断
        register::sie::set_stimer();
    }
}

#[unsafe(no_mangle)]
pub fn trap_handler() {
    println!("[trap_handler] Entering trap handler");
    set_kernel_trap_entry();
    let cx = current_trap_cx();
    let scause_val = register::scause::read();
    let interrupt_type = scause_val.cause();
    // 在发生缺页异常时，保存导致问题的虚拟地址
    let stval = stval::read();
    println!("[trap_handler] scause={:?}, stval={:#x}", interrupt_type, stval);

    if let Trap::Interrupt(code) = interrupt_type {
        println!("[trap_handler] Handling interrupt: {:?}", code);
        if let Ok(interrupt) = Interrupt::from_number(code) {
            match interrupt {
                Interrupt::SupervisorTimer => {
                    println!("[trap_handler] Timer interrupt");
                    timer::set_next_timer_interrupt();
                    suspend_current_and_run_next();
                }
                _ => {
                    panic!("Unknown interrupt: {:?}", interrupt);
                }
            }
        } else {
            panic!("Invalid interrupt code: {:?}", code);
        }
        return;
    }

    let original_sepc = cx.sepc;
    if let Trap::Exception(code) = interrupt_type {
        println!("[trap_handler] Handling exception: {:?}", code);
        if let Ok(exception) = Exception::from_number(code) {
            match exception {
                Exception::IllegalInstruction => {
                    println!("[kernel] IllegalInstruction in application, kernel killed it.");
                    exit_current_and_run_next();
                }
                Exception::Breakpoint => {
                    // ebreak 指令，如果是标准的 ebreak (opcode 00100000000000000000000001110011), 它是 32-bit (4 bytes) 的。
                    // 如果是压缩指令集中的 c.ebreak (opcode 1001000000000010), 它是 16-bit (2 bytes) 的。
                    // 一个简单（但不完全鲁棒）的判断方法是检查指令的低两位：如果指令的低两位是 11，它是一个 32-bit 或更长的指令。
                    // 如果不是 11 (即 00, 01, 10)，它是一个 16-bit 压缩指令。
                    // 所以，对于 ebreak 或非法指令，如果需要跳过它，sepc 应该增加 2 或 4。
                    println!("[trap_handler] Breakpoint exception");
                    cx.sepc += if (original_sepc & 0b11) != 0b11 { 2 } else { 4 };
                }
                Exception::UserEnvCall => {
                    println!("[trap_handler] User syscall: syscall_id={}, args=[{:#x}, {:#x}, {:#x}]",
                            cx.x[17], cx.x[10], cx.x[11], cx.x[12]);
                    cx.sepc += if (original_sepc & 0b11) != 0b11 { 2 } else { 4 };
                    let ret = syscall::syscall(cx.x[17], [cx.x[10], cx.x[11], cx.x[12]]);
                    cx.x[10] = ret as usize;
                    println!("[trap_handler] Syscall return value: {}", ret);
                }
                Exception::InstructionPageFault => {
                    // 当 CPU 的取指单元 (Instruction Fetch Unit) 试图从一个虚拟地址获取下一条要执行的指令时，
                    // 如果该虚拟地址的转换失败或权限不足，就会发生指令缺页异常
                    panic!("Instruction Page Fault, VA:{:#x}", stval);
                }
                Exception::LoadFault
                | Exception::LoadPageFault
                | Exception::StoreFault
                | Exception::StorePageFault => {
                    println!(
                        "[kernel] PageFault in application, bad addr = {:#x}, bad instruction = {:#x}, kernel killed it.",
                        stval, cx.sepc
                    );
                    exit_current_and_run_next();
                }
                _ => {
                    panic!("Trap exception: {:?} Not implemented", exception);
                }
            }
        } else {
            panic!("Invalid exception code: {:?}", code);
        }
        println!("[trap_handler] Trap handling completed, preparing to return");
        return;
    }

    panic!("Can not handle trap: scause: {:?}", scause_val);
}

fn set_kernel_trap_entry() {
    let mut val = stvec::Stvec::from_bits(0);
    val.set_address(trap_from_kernel as usize);
    val.set_trap_mode(TrapMode::Direct);
    unsafe {
        stvec::write(val);
    }
}

fn set_user_trap_entry() {
    let mut val = stvec::Stvec::from_bits(0);
    val.set_address(TRAMPOLINE);
    val.set_trap_mode(TrapMode::Direct);
    unsafe {
        stvec::write(val);
    }
}

#[unsafe(no_mangle)]
pub fn trap_return() -> ! {
    println!("[trap_return] Preparing to return to user mode");
    set_user_trap_entry();

    let trap_cx_ptr = TRAP_CONTEXT;
    let user_satp = current_user_token();
    println!("[trap_return] trap_cx_ptr={:#x}, user_satp={:#x}", trap_cx_ptr, user_satp);

    // 检查 TrapContext 的内容
    let trap_cx = current_trap_cx();
    println!("[trap_return] TrapContext content:");
    println!("  sepc={:#x}, sp={:#x}", trap_cx.sepc, trap_cx.x[2]);
    println!("  sstatus={:?}", trap_cx.sstatus);
    println!("  kernel_satp={:#x}, kernel_sp={:#x}", trap_cx.kernel_satp, trap_cx.kernel_sp);

    unsafe extern "C" {
        fn __restore();
        fn __alltraps();
    }
    let restore_va = __restore as usize - __alltraps as usize + TRAMPOLINE;
    println!("[trap_return] __restore={:#x}, __alltraps={:#x}", __restore as usize, __alltraps as usize);
    println!("[trap_return] TRAMPOLINE={:#x}", TRAMPOLINE);
    println!("[trap_return] restore_va={:#x}, about to jump to user program", restore_va);

    unsafe {
        asm!(
            "fence.i",
            "jr {restore_va}",
            restore_va = in(reg) restore_va,
            in("x10") trap_cx_ptr,
            in("x11") user_satp,
            options(noreturn)
        )
    }
}

#[unsafe(no_mangle)]
pub fn trap_from_kernel() -> ! {
    println!(
        "[trap_from_kernel] scause={:?}, stval={:#x}, sepc={:#x}",
        scause::read(),
        stval::read(),
        sepc::read()
    );
    panic!("a trap from kernel");
}
