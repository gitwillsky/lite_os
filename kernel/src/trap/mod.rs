pub mod context;
pub mod softirq;

use core::{arch::asm, panic};

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
    drivers::device_manager::{self},
    memory::TRAMPOLINE,
    signal::{SIG_RETURN_ADDR, handle_signals, sig_return},
    syscall,
    task::{self, exit_current_and_run_next},
    timer,
};

#[inline(always)]
fn clear_ssip() {
    unsafe { register::sip::clear_ssoft() }
}

#[inline(always)]
fn handle_supervisor_soft_interrupt() {
    clear_ssip();
    // 普通 IPI 只负责唤醒；TLB 同步由 SBI RFENCE 在 M-mode 完成。
    softirq::dispatch_current_cpu();
}

pub fn init() {
    set_kernel_trap_entry();
}

#[unsafe(no_mangle)]
pub fn trap_handler() {
    set_kernel_trap_entry();

    let scause_val = register::scause::read();
    let interrupt_type = scause_val.cause();
    // 在发生缺页异常时，保存导致问题的虚拟地址
    let stval = stval::read();

    if let Trap::Interrupt(code) = interrupt_type {
        if let Ok(interrupt) = Interrupt::from_number(code) {
            match interrupt {
                Interrupt::SupervisorTimer => {
                    // 仅做最小工作：重置下一次中断 + 通过 per-CPU softirq 登记并触发SSIP
                    timer::set_next_timer_interrupt();
                    softirq::raise(softirq::SoftIrq::Timer);
                }
                Interrupt::SupervisorExternal => {
                    device_manager::handle_external_interrupt();
                }
                Interrupt::SupervisorSoft => {
                    handle_supervisor_soft_interrupt();
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
                    if let Some(current) = task::current_task() {
                        let sepc = current.load_trap_context().sepc;
                        error!(
                            "[kernel] IllegalInstruction in application at PC:{:#x}, kernel killed it.",
                            sepc
                        );
                    } else {
                        error!("[kernel] IllegalInstruction with no current task");
                    }
                    exit_current_and_run_next(-2);
                }
                Exception::Breakpoint => {
                    // 在尚未实现标准 SIGTRAP frame 前不能猜测 16/32-bit 指令长度并跳过断点。
                    // 该 trap 完全由用户输入产生，只终止当前任务，不得 panic kernel。
                    error!("[kernel] breakpoint in application, terminating current task");
                    exit_current_and_run_next(-5);
                }
                Exception::UserEnvCall => {
                    if let Some(current) = task::current_task() {
                        // 1. 不允许 TrapContext 引用跨越系统调用；execve 会替换其底层地址空间。
                        let mut cx = current.load_trap_context();
                        let syscall_id = cx.x[17];
                        let args = [cx.x[10], cx.x[11], cx.x[12], cx.x[13], cx.x[14], cx.x[15]];
                        cx.sepc += 4;
                        current.set_trap_context(cx);
                        // sys_exit 不返回；若保留该 Arc，它会永久留在即将释放的 task stack 上。
                        drop(current);
                        let result = syscall::syscall(syscall_id, args);
                        let current = task::current_task()
                            .expect("returning syscall must still have a current task");

                        // 2. execve 成功时，新 TrapContext 已包含新程序入口；覆盖它会让 PC 回到旧映像。
                        let mut cx = current.load_trap_context();
                        if syscall_id != 221 || result != 0 {
                            cx.x[10] = result as usize;
                        }

                        // pending fatal signal 也可能不返回；signal helper 必须成为唯一 task-stack owner。
                        drop(current);
                        if !check_signals_and_maybe_exit_with_cx(&mut cx) {
                            // Process was terminated, should not continue
                            return;
                        }
                        let current = task::current_task()
                            .expect("signal check returned without a current task");
                        current.set_trap_context(cx);
                    } else {
                        error!("[kernel] UserEnvCall with no current task, terminating");
                        panic!("UserEnvCall with no current task");
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
                        let mut cx = task.load_trap_context();

                        match sig_return(&task, &mut cx) {
                            Ok(()) => {
                                task.set_trap_context(cx);
                                debug!("Sigreturn successful, continuing execution");
                                // 成功恢复，继续执行
                            }
                            Err(_) => {
                                error!("Sigreturn failed, terminating process");
                                // terminal switch 前必须释放 trap stack 上的所有 TCB owner。
                                drop(task);
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
                    if let Some(current) = task::current_task() {
                        let sepc_val = current.load_trap_context().sepc;
                        let sstatus_val = riscv::register::sstatus::read();
                        error!(
                            "[kernel] {:?} in application, bad addr = {:#x}, sepc = {:#x}, sstatus = {:#x}, core dumped.",
                            scause_val,
                            stval,
                            sepc_val,
                            sstatus_val.bits(),
                        );
                    } else {
                        error!(
                            "[kernel] {:?} with no current task, bad addr = {:#x}, core dumped.",
                            scause_val, stval,
                        );
                    }
                    exit_current_and_run_next(-5);
                }
                _ => {
                    error!(
                        "[kernel] unsupported application exception {:?}, stval={:#x}",
                        exception, stval
                    );
                    exit_current_and_run_next(-5);
                }
            }
        } else {
            error!(
                "[kernel] invalid application exception code {:?}, stval={:#x}",
                code, stval
            );
            exit_current_and_run_next(-5);
        }
    }

    // IPI/interrupt 返回前处理 pending signal；Running → Stopped/Exited 只能由 owner hart 完成。
    if let Some(current) = task::current_task()
        && crate::signal::has_pending_signals(&current)
    {
        let mut cx = current.load_trap_context();
        drop(current);
        check_signals_and_maybe_exit_with_cx(&mut cx);
        let current = task::current_task().expect("signal handling returned without current task");
        current.set_trap_context(cx);
    }

    // kernel/user timer softirq 共用该 flag；只在即将返回用户态时切换，避免在 hardirq 中调度。
    if task::take_reschedule() && task::current_task().is_some() {
        task::suspend_current_and_run_next();
    }
    trap_return();
}

/// Helper function to check and handle pending signals with existing trap context
/// Returns true if execution should continue, false if process should exit
fn check_signals_and_maybe_exit_with_cx(trap_cx: &mut TrapContext) -> bool {
    if let Some(task) = task::current_task() {
        let (should_continue, exit_code) = handle_signals(&task, Some(trap_cx));
        if !should_continue {
            if let Some(code) = exit_code {
                // fatal signal 不返回；否则此局部 Arc 会永久留在 exiting task stack。
                drop(task);
                exit_current_and_run_next(code);
            } else {
                // 进程被信号停止（如SIGTSTP），需要暂停当前进程并切换到其他进程
                // stopped task 可能在恢复前被终止；不能把 owning Arc 留在它的 suspended stack。
                drop(task);
                task::stop_current_and_run_next();
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
    // 关键修复：先关闭中断防止任务切换，然后获取必要信息
    unsafe {
        riscv::register::sstatus::clear_sie();
    }

    // 简化的原子获取方案：最小化锁持有时间
    let current_task = crate::task::current_task().expect("No current task in trap_return");
    let user_satp = current_task.user_token();
    let trap_context_va = current_task.trap_context_va();
    let kernel_gp: usize;
    unsafe {
        asm!("mv {}, gp", out(reg) kernel_gp, options(nomem, nostack));
    }
    {
        let mut trap_context = current_task.load_trap_context();
        trap_context.kernel_hart_id = crate::arch::hart::hart_id();
        trap_context.kernel_gp = kernel_gp;
        current_task.set_trap_context(trap_context);
    }

    unsafe extern "C" {
        fn __restore();
        fn __alltraps();
    }
    let restore_va = __restore as usize - __alltraps as usize + TRAMPOLINE;

    // 设置用户陷阱入口
    set_user_trap_entry();

    unsafe {
        asm!(
            "fence.i",
            "jr {restore_va}",
            restore_va = in(reg) restore_va,
            in("x10") trap_context_va,
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
                        // kernel/user timer 使用同一 per-hart softirq；hardirq 不扫描任务表或分配。
                        softirq::raise(softirq::SoftIrq::Timer);
                    }
                    Interrupt::SupervisorExternal => {
                        // 在内核态也处理外部中断（如 VirtIO 块设备完成中断），
                        // 以便唤醒内核态等待 I/O 的任务，避免死等导致看门狗触发。
                        device_manager::handle_external_interrupt();
                    }
                    Interrupt::SupervisorSoft => {
                        handle_supervisor_soft_interrupt();
                    }
                    _ => {
                        panic!("Unhandled kernel interrupt: {:?}", interrupt);
                    }
                }
            } else {
                panic!("Invalid kernel interrupt code: {:?}", code);
            }
        }
        Trap::Exception(code) => {
            if let Ok(exception) = Exception::from_number(code) {
                // 交由统一的 panic 冻结/快照路径输出更详细的信息
                panic!(
                    "Kernel exception: {:?}, scause={:?}, stval={:#x}, sepc={:#x}",
                    exception, scause_val, stval_val, sepc_val
                );
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
