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
    memory::{TRAMPOLINE, TRAP_CONTEXT, KERNEL_SPACE, address::VirtualAddress},
    signal::{self, handle_signals, sig_return, SIG_RETURN_ADDR},
    syscall,
    task::{
        self, current_user_token, exit_current_and_run_next, mark_kernel_entry,
        mark_kernel_exit, suspend_current_and_run_next,
    },
    timer, watchdog,
};

global_asm!(include_str!("trap.S"));

pub fn init() {
    set_kernel_trap_entry();
}

#[unsafe(no_mangle)]
pub fn trap_handler() {
    set_kernel_trap_entry();

    // 标记进入内核态
    mark_kernel_entry();

    let scause_val = register::scause::read();
    let interrupt_type = scause_val.cause();
    // 在发生缺页异常时，保存导致问题的虚拟地址
    let stval = stval::read();

    if let Trap::Interrupt(code) = interrupt_type {
        if let Ok(interrupt) = Interrupt::from_number(code) {
            match interrupt {
                Interrupt::SupervisorTimer => {
                    timer::set_next_timer_interrupt();

                    // 检查 watchdog 状态
                    watchdog::check();

                    // 检查并唤醒到期的睡眠任务
                    task::check_and_wakeup_sleeping_tasks(timer::get_time_ns());

                    // Check and handle pending signals before task switch
                    {
                        let cx = task::current_trap_context();
                        if !check_signals_and_maybe_exit_with_cx(cx) {
                            // Process was terminated, should not continue
                            return;
                        }
                    }

                    suspend_current_and_run_next();
                }
                Interrupt::SupervisorExternal => {
                    // 处理外部中断（包括VirtIO设备中断）
                    crate::drivers::handle_external_interrupt();
                }
                Interrupt::SupervisorSoft => {
                    // 读取当前sip寄存器值
                    let sip_val: usize;
                    unsafe {
                        asm!("csrr {}, sip", out(reg) sip_val);
                    }

                    // 清除SSIP位（位1）
                    let clear_ssip = sip_val & !(1 << 1);
                    unsafe {
                        asm!("csrw sip, {}", in(reg) clear_ssip);
                    }

                    // 检查当前进程是否有待处理的信号
                    let cx = task::current_trap_context();
                    if !check_signals_and_maybe_exit_with_cx(cx) {
                        // Process was terminated, should not continue
                        return;
                    }
                }
                _ => {
                    panic!("Unknown interrupt: {:?} (code: {})", interrupt, code);
                }
            }
        } else {
            panic!("Invalid interrupt code: {:?}", code);
        }
    } else if let Trap::Exception(code) = interrupt_type {
        if let Ok(exception) = Exception::from_number(code) {
            match exception {
                Exception::IllegalInstruction => {
                    let sepc = task::current_trap_context().sepc;
                    error!(
                        "[kernel] IllegalInstruction in application at PC:{:#x}, kernel killed it.",
                        sepc
                    );
                    exit_current_and_run_next(-2);
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

                    cx.x[10] = {
                        cx.sepc += 4;
                        syscall::syscall(syscall_id, args) as usize
                    };

                    // Check and handle pending signals after syscall using the existing trap context
                    if !check_signals_and_maybe_exit_with_cx(cx) {
                        // Process was terminated, should not continue
                        return;
                    }
                }
                Exception::InstructionPageFault => {
                    // 当 CPU 的取指单元 (Instruction Fetch Unit) 试图从一个虚拟地址获取下一条要执行的指令时，
                    // 如果该虚拟地址的转换失败或权限不足，就会发生指令缺页异常

                    // 检查是否是信号处理函数返回时的特殊情况 (地址为0)
                    if stval == SIG_RETURN_ADDR {
                        // 这是信号处理函数返回的特殊情况，调用sigreturn恢复原始上下文
                        debug!("Signal handler return detected (VA=0), calling sigreturn");
                        let task = task::current_task().expect("No current task");
                        let mut cx = task::current_trap_context();

                        match sig_return(&task, cx) {
                            Ok(()) => {
                                debug!("Sigreturn successful, continuing execution");
                                // 成功恢复，继续执行
                            }
                            Err(_) => {
                                error!("Sigreturn failed, terminating process");
                                exit_current_and_run_next(-5);
                            }
                        }
                    } else {
                        error!("Instruction Page Fault, VA:{:#x}", stval);
                        exit_current_and_run_next(-5);
                    }
                }
                Exception::LoadFault
                | Exception::LoadPageFault
                | Exception::StoreFault
                | Exception::StorePageFault => {
                    error!(
                        "[kernel] {:?} in application, bad addr = {:#x}, bad instruction = {:#x}, core dumped.",
                        scause_val,
                        stval,
                        task::current_trap_context().sepc,
                    );
                }
                _ => {
                    panic!("Trap exception: {:?} Not implemented", exception);
                }
            }
        } else {
            panic!("Invalid exception code: {:?}", code);
        }
    }

    // 标记退出内核态
    mark_kernel_exit();

    trap_return();
}

/// Helper function to check and handle pending signals
/// Returns true if execution should continue, false if process should exit
fn check_signals_and_maybe_exit() -> bool {
    if let Some(task) = task::current_task() {
        let (should_continue, exit_code) = handle_signals(&task, None);
        if !should_continue {
            if let Some(code) = exit_code {
                exit_current_and_run_next(code);
                // This function may return if there's no other task to run
                // In that case, we should end execution anyway
                return false;
            } else {
                // Process stopped, no trap context available for restoration
                task::suspend_current_and_run_next();
                // Process was suspended and then resumed, continue execution
                return true;
            }
        }
        should_continue
    } else {
        true
    }
}

/// Helper function to check and handle pending signals with existing trap context
/// Returns true if execution should continue, false if process should exit
fn check_signals_and_maybe_exit_with_cx(trap_cx: &mut TrapContext) -> bool {
    if let Some(task) = task::current_task() {
        let (should_continue, exit_code) = handle_signals(&task, Some(trap_cx));
        if !should_continue {
            if let Some(code) = exit_code {
                exit_current_and_run_next(code);
                // This function may return if there's no other task to run
                // In that case, we should end execution anyway
                return false;
            } else {
                // 进程被信号停止（如SIGTSTP），需要暂停当前进程并切换到其他进程
                task::suspend_current_and_run_next();
                // Process was suspended and then resumed, continue execution
                return true;
            }
        }
        should_continue
    } else {
        true
    }
}

fn set_kernel_trap_entry() {
    let mut val = stvec::Stvec::from_bits(0);
    unsafe extern "C" {
        fn __kernel_trap();
    }
    val.set_address(__kernel_trap as usize);
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
    let user_satp = current_user_token();
    unsafe extern "C" {
        fn __restore();
        fn __alltraps();
    }
    let restore_va = __restore as usize - __alltraps as usize + TRAMPOLINE;

    set_user_trap_entry();

    unsafe {
        asm!(
            "fence.i",
            "jr {restore_va}",
            restore_va = in(reg) restore_va,
            in("x10") crate::task::current_task().unwrap().trap_context_va(),
            in("x11") user_satp,
            options(noreturn)
        )
    }
}

#[unsafe(no_mangle)]
extern "C" fn rust_trap_from_kernel() {
    let scause_val = scause::read();
    let stval_val = stval::read();
    let sepc_val = sepc::read();

    match scause_val.cause() {
        Trap::Interrupt(code) => {
            if let Ok(interrupt) = Interrupt::from_number(code) {
                match interrupt {
                    Interrupt::SupervisorTimer => {
                        timer::set_next_timer_interrupt();
                        watchdog::check();
                        // 在内核态可能没有 current_task，避免访问 TrapContext
                        let _ = task::check_and_wakeup_sleeping_tasks(timer::get_time_ns());
                        // 不做任务切换，仅返回，让普通调度循环运行
                    }
                    Interrupt::SupervisorExternal => {
                        // 在内核态也处理外部中断（如 VirtIO 块设备完成中断），
                        // 以便唤醒内核态等待 I/O 的任务，避免死等导致看门狗触发。
                        crate::drivers::handle_external_interrupt();
                    }
                    Interrupt::SupervisorSoft => {
                        // 清SSIP
                        let sip_val: usize;
                        unsafe { asm!("csrr {}, sip", out(reg) sip_val); }
                        let clear_ssip = sip_val & !(1 << 1);
                        unsafe { asm!("csrw sip, {}", in(reg) clear_ssip); }
                    }
                    _ => {
                        error!("[kernel] Unhandled kernel interrupt: {:?}", interrupt);
                    }
                }
            } else {
                error!("[kernel] Invalid kernel interrupt code: {:?}", code);
            }
        }
        Trap::Exception(code) => {
            if let Ok(exception) = Exception::from_number(code) {
                // 辅助调试：尝试解码 sepc/stval 的内核页表映射与 PTE 标志
                let (sepc_map, stval_map, sepc_flags, stval_flags) = {
                    let kernel_space = KERNEL_SPACE.wait().lock();
                    let sepc_va = VirtualAddress::from(sepc_val);
                    let stval_va = VirtualAddress::from(stval_val);
                    let sepc_pa = kernel_space.translate_va(sepc_va);
                    let stval_pa = kernel_space.translate_va(stval_va);
                    let sepc_pte = kernel_space.translate(sepc_va.floor());
                    let stval_pte = kernel_space.translate(stval_va.floor());
                    (
                        sepc_pa,
                        stval_pa,
                        sepc_pte.map(|p| p.flags()),
                        stval_pte.map(|p| p.flags()),
                    )
                };
                error!(
                    "[trap_from_kernel] exception={:?}, scause={:?}, stval={:#x} (PA={:?}, PTE={:?}), sepc={:#x} (PA={:?}, PTE={:?})",
                    exception, scause_val, stval_val, stval_map, stval_flags, sepc_val, sepc_map, sepc_flags
                );
                panic!("Kernel exception: {:?}", exception);
            } else {
                error!(
                    "[trap_from_kernel] invalid exception code: {:?}, scause={:?}, stval={:#x}, sepc={:#x}",
                    code, scause_val, stval_val, sepc_val
                );
                panic!("Kernel invalid exception code: {:?}", code);
            }
        }
    }
}
