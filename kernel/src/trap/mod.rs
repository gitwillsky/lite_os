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
    task::{self, exit_current_and_run_next, suspend_current_and_run_next},
    timer,
};

global_asm!(include_str!("trap.S"));

pub fn init() {
    set_kernel_trap_entry();
}

#[unsafe(no_mangle)]
pub fn trap_handler() -> ! {
    set_kernel_trap_entry();
    let scause = scause::read();
    let stval = stval::read();
    let sepc = sepc::read();

    debug!("[trap_handler] Trap occurred: cause={:?}, stval={:#x}, sepc={:#x}",
           scause.cause(), stval, sepc);

    match scause.cause() {
        Trap::Interrupt(code) => {
            if let Ok(interrupt) = Interrupt::from_number(code) {
                match interrupt {
                    Interrupt::SupervisorTimer => {
                        timer::set_next_timer_interrupt();
                        // 处理定时器任务（如alarm等）
                        timer::handle_timer_tasks();

                        // 检查是否需要进行调度
                        if task::should_schedule() {
                            suspend_current_and_run_next();
                        }
                    }
                    Interrupt::SupervisorExternal => {
                        // 处理外部中断（包括VirtIO设备中断）
                        debug!("[trap_handler] External interrupt");
                        crate::drivers::handle_external_interrupt();
                    }
                    _ => {
                        panic!("Unknown interrupt: {:?}", interrupt);
                    }
                }
            } else {
                panic!("Invalid interrupt code: {:?}", code);
            }
        }
        Trap::Exception(code) => {
            if let Ok(exception) = Exception::from_number(code) {
                match exception {
                    Exception::IllegalInstruction => {
                        error!("[kernel] IllegalInstruction in application, kernel killed it.");
                        exit_current_and_run_next(-3);
                    }
                    Exception::Breakpoint => {
                        // ebreak 指令，如果是标准的 ebreak (opcode 00100000000000000000000001110011), 它是 32-bit (4 bytes) 的。
                        // 如果是压缩指令集中的 c.ebreak (opcode 1001000000000010), 它是 16-bit (2 bytes) 的。
                        // 一个简单（但不完全鲁棒）的判断方法是检查指令的低两位：如果指令的低两位是 11，它是一个 32-bit 或更长的指令。
                        // 如果不是 11 (即 00, 01, 10)，它是一个 16-bit 压缩指令。
                        // 所以，对于 ebreak 或非法指令，如果需要跳过它，sepc 应该增加 2 或 4。
                        debug!("[trap_handler] Breakpoint exception");
                        let cx = task::current_trap_context();
                        cx.sepc += 4;
                    }
                    Exception::UserEnvCall => {
                        let cx = task::current_trap_context();
                        let syscall_id = cx.x[17];
                        let args = [cx.x[10], cx.x[11], cx.x[12]];

                        // Only debug important syscalls
                        if syscall_id == 64 || syscall_id == 700 || syscall_id == 702 || syscall_id == 703 {
                            debug!("[trap_handler] SystemCall: syscall_id={}, args=[{:#x}, {:#x}, {:#x}]",
                                   syscall_id, args[0], args[1], args[2]);
                        }

                        cx.sepc += 4;
                        let ret = syscall::syscall(syscall_id, args);

                        // sys_exec change the TrapContext, we need reload it
                        let cx = task::current_trap_context();

                        cx.x[10] = ret as usize;

                        if syscall_id == 64 || syscall_id == 700 || syscall_id == 702 || syscall_id == 703 {
                            debug!("[trap_handler] SystemCall completed: syscall_id={}, ret={}", syscall_id, ret);
                        }
                    }
                    Exception::InstructionPageFault => {
                        // 检查是否是信号处理函数返回
                        let sepc = {
                            let cx = task::current_trap_context();
                            cx.sepc
                        }; // cx借用在这里结束

                        if sepc == 0 {
                            // 这是信号处理函数返回，自动调用sigreturn
                            debug!("[trap_handler] Signal handler return detected, calling sigreturn");
                            if syscall::sys_sigreturn() == 0 {
                                trap_return();
                            } else {
                                error!("[kernel] sigreturn failed, killing process.");
                                exit_current_and_run_next(-4);
                            }
                        }

                        // 当 CPU 的取指单元 (Instruction Fetch Unit) 试图从一个虚拟地址获取下一条要执行的指令时，
                        // 如果该虚拟地址的转换失败或权限不足，就会发生指令缺页异常
                        panic!("Instruction Page Fault, VA:{:#x}", stval);
                    }
                    Exception::LoadFault
                    | Exception::LoadPageFault
                    | Exception::StoreFault
                    | Exception::StorePageFault => {
                        error!(
                            "[kernel] {:?} in application, bad addr = {:#x}, bad instruction = {:#x}, core dumped.",
                            scause.cause(),
                            stval,
                            task::current_trap_context().sepc,
                        );
                        exit_current_and_run_next(-2);
                    }
                    _ => {
                        panic!("Trap exception: {:?} Not implemented", exception);
                    }
                }
            } else {
                panic!("Invalid exception code: {:?}", code);
            }
        }
    }
    trap_return();
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
    debug!("[trap_return] Called, about to return to user space");

    // 在返回用户态之前检查信号
    if let Some(task) = task::current_task() {
        let (should_continue, exit_code) = crate::task::check_and_handle_signals();
        if !should_continue {
            if let Some(code) = exit_code {
                debug!("[trap_return] Exiting due to signal with code: {}", code);
                exit_current_and_run_next(code);
            }
        }
    } else {
        error!("[trap_return] No current task!");
        panic!("trap_return called with no current task");
    }

    set_user_trap_entry();

    let trap_cx_ptr = TRAP_CONTEXT;
    let user_satp = task::current_user_token();

    // 检查trap context状态
    let trap_cx = task::current_trap_context();
    debug!("[trap_return] Final trap context check - sepc: {:#x}, sp: {:#x}, s0: {:#x}",
           trap_cx.sepc, trap_cx.x[2], trap_cx.x[8]);

    // 验证返回地址是否在合理范围内并且页面已映射
    if let Some(current_task) = task::current_task() {
        let task_inner = current_task.inner_exclusive_access();
        let memory_set = &task_inner.mm.memory_set;

        // 检查 sepc (程序计数器) 的页面映射
        let sepc_addr = trap_cx.sepc & !0xfff; // 页面对齐
        let sepc_vpn = crate::memory::address::VirtualPageNumber::from(crate::memory::address::VirtualAddress::from(sepc_addr));
        if let Some(pte) = memory_set.translate(sepc_vpn) {
            debug!("[trap_return] sepc {:#x} (page {:#x}) is mapped to ppn {:#x}, flags: {:?}",
                   trap_cx.sepc, sepc_addr, pte.ppn().as_usize(), pte.flags());
        } else {
            error!("[trap_return] ERROR: sepc {:#x} (page {:#x}) is NOT mapped!", trap_cx.sepc, sepc_addr);
        }

        // 检查栈指针的页面映射
        let sp_addr = trap_cx.x[2] & !0xfff; // 页面对齐
        let sp_vpn = crate::memory::address::VirtualPageNumber::from(crate::memory::address::VirtualAddress::from(sp_addr));
        if let Some(pte) = memory_set.translate(sp_vpn) {
            debug!("[trap_return] sp {:#x} (page {:#x}) is mapped to ppn {:#x}, flags: {:?}",
                   trap_cx.x[2], sp_addr, pte.ppn().as_usize(), pte.flags());
        } else {
            error!("[trap_return] ERROR: sp {:#x} (page {:#x}) is NOT mapped!", trap_cx.x[2], sp_addr);
        }

        drop(task_inner);
    }

    unsafe extern "C" {
        fn __restore();
        fn __alltraps();
    }
    let restore_va = __restore as usize - __alltraps as usize + TRAMPOLINE;

    debug!("[trap_return] About to execute assembly: restore_va={:#x}, trap_cx_ptr={:#x}, user_satp={:#x}",
           restore_va, trap_cx_ptr, user_satp);
    debug!("[trap_return] Final check - will jump to sepc={:#x} with sp={:#x}",
           trap_cx.sepc, trap_cx.x[2]);

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
    error!(
        "[trap_from_kernel] scause={:?}, stval={:#x}, sepc={:#x}",
        scause::read(),
        stval::read(),
        sepc::read()
    );
    panic!("a trap from kernel");
}
