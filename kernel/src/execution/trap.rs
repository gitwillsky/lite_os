//! trap 分派：把架构解码出的语义事件交给对应 owner，自己不解释寄存器或用户态语义。
//!
//! # 用户态 trap
//!
//! 1. 重新安装内核态 trap 入口。
//! 2. 中断只确认硬件并登记 deferred work；syscall 与同步异常交给当前线程的 [`ThreadOwner`]。
//!    RISC-V 的惰性 FP 激活是唯一由架构自身消化的异常，原指令随后重试。
//! 3. 汇合点：所有用户态 trap 在 owner 释放锁之后统一处理一次 deferred work。
//! 4. 调度标志置位时在这里切换；抢占只发生在即将返回用户态时，不在中断处理中。
//! 5. [`trap_return`]：owner 完成返回前处理（可能不返回），然后关中断、激活地址空间、经 trampoline 返回。
//!
//! # 中断确认顺序
//!
//! 时钟中断必须先重置时钟源再 EOI，反序会让中断控制器立即重投同一中断；核间中断必须先 EOI、
//! 清除本地 pending 边沿，再读取所有请求，否则远端在两步之间发布的请求可能被合并进旧边沿而丢失。
//!
//! [`ThreadOwner`]: crate::thread::ThreadOwner

use crate::arch::{self, trap::TrapEvent};
use crate::execution::deferred::{self, DeferredWork};
use crate::execution::timer;
use crate::thread::{self, Disposition, UserException};
use crate::vm::FaultAccess;

fn handle_software_interrupt() {
    // RISC-V SSIP 必须先清除再完成同步请求；两步是唯一的 trap 确认点。
    arch::interrupt::clear_software();
    crate::crash::stop_if_requested();
}

/// timer tick 的 deferred work；`user_trap` 表示在用户态 trap 入口响应。
fn timer_work(user_trap: bool, fired: u64) -> DeferredWork {
    if user_trap && timer::tick_interrupted_user(fired) {
        DeferredWork::TIMER.with(DeferredWork::TIMER_FROM_USER)
    } else {
        DeferredWork::TIMER
    }
}

fn handle_claimed_interrupt(user_trap: bool) {
    let claimed = crate::platform::claim_interrupt();
    let software = matches!(&claimed, crate::platform::ClaimedInterrupt::Software(_));
    match &claimed {
        crate::platform::ClaimedInterrupt::Timer(_) => {
            // 先重置 level timer source，再 EOI；反序会让 GIC 立即重投同一 PPI。
            let fired = timer::set_next_timer_interrupt();
            crate::crash::watchdog_tick();
            deferred::raise(timer_work(user_trap, fired));
        }
        crate::platform::ClaimedInterrupt::Device(_) | crate::platform::ClaimedInterrupt::Software(_) => {}
        crate::platform::ClaimedInterrupt::Spurious => return,
    }
    // 设备 handler 已在 claim 内完成；token 必须恰好一次交还同一控制器。
    crate::platform::complete_interrupt(claimed);
    if software {
        // 必须先 EOI/清除本地 pending 边沿，再读取全部 SGI 请求。
        crate::platform::complete_pending_ipi();
        crate::crash::stop_if_requested();
    }
}

fn handle_interrupt(event: TrapEvent, user_trap: bool) -> bool {
    match event {
        TrapEvent::TimerInterrupt => {
            let fired = timer::set_next_timer_interrupt();
            crate::crash::watchdog_tick();
            deferred::raise(timer_work(user_trap, fired));
        }
        TrapEvent::ExternalInterrupt => {
            handle_claimed_interrupt(user_trap);
            if crate::log::serial_input_pending() {
                deferred::raise(DeferredWork::CONSOLE);
            }
        }
        TrapEvent::SoftwareInterrupt => handle_software_interrupt(),
        TrapEvent::UnsupportedInterrupt => panic!("unsupported interrupt"),
        _ => return false,
    }
    true
}

pub(crate) fn init() {
    arch::trap::install_kernel_entry();
}

/// 用户态 trap 的唯一入口。
pub(crate) fn handle_user_trap() -> ! {
    arch::trap::install_kernel_entry();
    let event = arch::trap::event();
    if !handle_interrupt(event, true) {
        let current = thread::current().expect("user trap without a current thread");
        let owner = current.owner().clone();
        let disposition = match event {
            TrapEvent::UserEnvironmentCall => owner.syscall(&current),
            TrapEvent::TimerInterrupt
            | TrapEvent::ExternalInterrupt
            | TrapEvent::SoftwareInterrupt
            | TrapEvent::UnsupportedInterrupt => unreachable!("interrupt handled above"),
            event => match user_exception(&current, event) {
                Some(exception) => owner.exception(&current, exception),
                None => Disposition::Continue,
            },
        };
        // 退出不返回；必须先释放本帧持有的引用（owner 已随调用移交并释放）。
        drop(current);
        if disposition == Disposition::Exit {
            thread::exit_current();
        }
    }
    // 汇合点：owner 已释放全部锁，统一处理中断登记的 deferred work。
    thread::dispatch_deferred();
    if thread::take_reschedule() && thread::current().is_some() {
        thread::yield_current();
    }
    trap_return()
}

fn user_exception(current: &alloc::sync::Arc<thread::Thread>, event: TrapEvent) -> Option<UserException> {
    match event {
        TrapEvent::IllegalInstruction => illegal_instruction(current),
        TrapEvent::Breakpoint => Some(UserException::Breakpoint {
            pc: current.with_user_context(|context| context.program_counter()),
        }),
        TrapEvent::InstructionPageFault { fault } => Some(page_fault(fault, FaultAccess::Execute)),
        TrapEvent::LoadPageFault { fault } => Some(page_fault(fault, FaultAccess::Read)),
        TrapEvent::StorePageFault { fault } => Some(page_fault(fault, FaultAccess::Write)),
        TrapEvent::LoadAccessFault { fault } | TrapEvent::StoreAccessFault { fault } => {
            Some(UserException::AccessFault {
                address: fault.address(),
                syndrome: fault.syndrome(),
            })
        }
        TrapEvent::AlignmentFault { fault } => Some(UserException::Alignment {
            address: fault.address(),
            syndrome: fault.syndrome(),
        }),
        TrapEvent::UnsupportedException { address } => Some(UserException::Unsupported { pc: address }),
        TrapEvent::UserEnvironmentCall
        | TrapEvent::TimerInterrupt
        | TrapEvent::ExternalInterrupt
        | TrapEvent::SoftwareInterrupt
        | TrapEvent::UnsupportedInterrupt => None,
    }
}

fn page_fault(fault: arch::trap::UserFault, access: FaultAccess) -> UserException {
    UserException::PageFault {
        address: fault.address(),
        access,
        syndrome: fault.syndrome(),
    }
}

/// 架构惰性状态（RISC-V FP）可消化的非法指令返回 `None` 并重试原指令。
fn illegal_instruction(current: &alloc::sync::Arc<thread::Thread>) -> Option<UserException> {
    let probe = current.with_user_context(|context| context.illegal_instruction_probe());
    let space = current.address_space();
    let result = arch::context::inspect_illegal_instruction(probe, |address, destination| {
        space
            .as_ref()
            .is_some_and(|space| space.copy_from_user(address, destination).is_ok())
    });
    match current.with_user_context(|context| context.finish_illegal_instruction(result)) {
        Ok(()) => None,
        Err(fault) => Some(UserException::IllegalInstruction { pc: fault.address() }),
    }
}

/// 用户线程返回用户态；首次调度的线程也从这里进入。
pub(crate) fn trap_return() -> ! {
    // owner 的返回前处理可能阻塞或要求退出，因此在建立最终 transfer window 之前完成；
    // 退出前释放本帧引用。
    let disposition = {
        let current = thread::current().expect("trap return without a current thread");
        let owner = current.owner().clone();
        owner.before_user_return(&current)
    };
    if disposition == Disposition::Exit {
        thread::exit_current();
    }
    arch::interrupt::disable_for_transfer();
    let current = thread::current().expect("trap return lost its current thread");
    let token = current
        .address_space()
        .expect("returning thread has no address space")
        .token();
    let context = current.prepare_user_return(crate::execution::cpu::current_id().index());
    // noreturn trampoline 不展开 Rust frame；不显式释放会把线程引用永久留在内核栈上。
    drop(current);
    timer::note_return_to_user();
    arch::trap::return_to_user(context, token, crate::arch::mmu::TRAMPOLINE_ADDRESS)
}

/// 内核态 trap：只接受中断；其他异常是内核自身的缺陷，fail-stop。
pub(crate) fn handle_kernel_trap() {
    let event = arch::trap::event();
    if !handle_interrupt(event, false) {
        panic!("kernel trap: {:?}", arch::trap::kernel_exception(event));
    }
}
