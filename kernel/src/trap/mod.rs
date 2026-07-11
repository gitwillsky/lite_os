use core::{arch::asm, panic};

use riscv::{
    ExceptionNumber, InterruptNumber,
    interrupt::{Exception, Interrupt, Trap},
    register::{
        self, scause, sepc, stval,
        stvec::{self, TrapMode},
    },
};
use syscall_abi::SYSCALL_EXECVE;

use crate::{
    arch::hart,
    drivers,
    memory::TRAMPOLINE,
    syscall,
    task::{self, SignalDelivery, exit_current_and_run_next},
    timer,
};

#[inline(always)]
fn handle_supervisor_soft_interrupt() {
    // 普通 IPI 只负责唤醒；TLB 同步由 SBI RFENCE 在 M-mode 完成。
    task::dispatch_pending_timer_work();
}

pub(crate) fn init() {
    set_kernel_trap_entry();
}

#[unsafe(no_mangle)]
pub(crate) fn trap_handler() {
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
                    hart::raise_timer_softirq();
                }
                Interrupt::SupervisorExternal => {
                    drivers::handle_external_interrupt();
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
                        if syscall_id != SYSCALL_EXECVE || result != 0 {
                            cx.x[10] = result as usize;
                        }

                        current.set_trap_context(cx);
                    } else {
                        error!("[kernel] UserEnvCall with no current task, terminating");
                        panic!("UserEnvCall with no current task");
                    }
                }
                Exception::InstructionPageFault => {
                    error!("Instruction Page Fault, VA:{:#x}", stval);
                    exit_current_and_run_next(-5);
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

    // kernel/user timer softirq 共用该 flag；只在即将返回用户态时切换，避免在 hardirq 中调度。
    if task::take_reschedule() && task::current_task().is_some() {
        task::suspend_current_and_run_next();
    }
    trap_return();
}

fn set_kernel_trap_entry() {
    let mut val = stvec::Stvec::from_bits(0);
    // SAFETY: assembly defines this aligned symbol with C linkage in the kernel trap text.
    unsafe extern "C" {
        fn __kernel_trap();
    }
    val.set_address(__kernel_trap as usize);
    val.set_trap_mode(TrapMode::Direct);
    // SAFETY: `__kernel_trap` is an aligned linked trap entry and this updates only local stvec.
    unsafe {
        stvec::write(val);
    }
}

fn set_user_trap_entry() {
    let mut val = stvec::Stvec::from_bits(0);
    val.set_address(TRAMPOLINE);
    val.set_trap_mode(TrapMode::Direct);
    // SAFETY: TRAMPOLINE is the aligned executable user-trap entry mapped in every address space.
    unsafe {
        stvec::write(val);
    }
}

#[unsafe(no_mangle)]
pub(crate) fn trap_return() -> ! {
    // 关键修复：先关闭中断防止任务切换，然后获取必要信息
    // SAFETY: trap return runs in S-mode and disables only current-hart SIE while assembling
    // the restore state.
    unsafe {
        riscv::register::sstatus::clear_sie();
    }

    // 简化的原子获取方案：最小化锁持有时间
    let current_task = crate::task::current_task().expect("No current task in trap_return");
    match current_task.prepare_signal_delivery() {
        Ok(SignalDelivery::None) => {}
        Ok(SignalDelivery::Terminate(status)) => {
            drop(current_task);
            exit_current_and_run_next(status);
        }
        Err(_) => {
            drop(current_task);
            exit_current_and_run_next(139);
        }
    }
    let user_satp = current_task.user_token();
    let trap_context_va = current_task.trap_context_va();
    let kernel_gp: usize;
    // SAFETY: reading `gp` has no memory effect and preserves the kernel global-pointer value
    // required by the trampoline on its next supervisor entry.
    unsafe {
        asm!("mv {}, gp", out(reg) kernel_gp, options(nomem, nostack));
    }
    {
        let mut trap_context = current_task.load_trap_context();
        trap_context.kernel_hart_id = crate::arch::hart::hart_id();
        trap_context.kernel_gp = kernel_gp;
        current_task.set_trap_context(trap_context);
    }

    // SAFETY: both symbols are emitted by the trampoline assembly in one section; their addresses
    // are used only to derive the mapped restore entry offset.
    unsafe extern "C" {
        fn __restore();
        fn __alltraps();
    }
    let restore_va = __restore as usize - __alltraps as usize + TRAMPOLINE;

    // 设置用户陷阱入口
    set_user_trap_entry();

    // SAFETY: restore_va points to linked trampoline code; trap_context_va and user_satp belong
    // to the current live task, and the jump never returns through this Rust frame.
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
                        hart::raise_timer_softirq();
                    }
                    Interrupt::SupervisorExternal => {
                        // 内核态 VirtIO 同步 I/O 可以被 external IRQ 打断；
                        // 此处只确认设备/PLIC 状态，不在 hardirq 中调度。
                        drivers::handle_external_interrupt();
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
