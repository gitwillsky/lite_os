use core::panic;
use syscall_abi::SYSCALL_EXECVE;

use crate::{
    arch::{self, context::SyscallCompletion, trap::TrapEvent},
    cpu::{self, DeferredWork},
    drivers,
    memory::TRAMPOLINE,
    syscall::{self, SyscallOutcome},
    task::{self, SignalDelivery, exit_current_group_by_signal, stop_current_process},
    timer,
};

#[inline(always)]
fn handle_supervisor_soft_interrupt() {
    // RISC-V SSIP 必须先 clear 再完成同步 barrier；两步是唯一 trap-owned ack seam。
    arch::interrupt::clear_software();
    crate::task::complete_pending_memory_barrier();
}

#[inline(always)]
fn handle_claimed_interrupt() {
    let claimed = crate::platform::claim_interrupt();
    let software = matches!(&claimed, crate::platform::ClaimedInterrupt::Software(_));
    match &claimed {
        crate::platform::ClaimedInterrupt::Timer(_) => {
            // 先重置 level timer source，再 EOI；反序会让 GIC 立即重投同一 PPI。
            timer::set_next_timer_interrupt();
            cpu::raise_deferred(DeferredWork::Timer);
        }
        crate::platform::ClaimedInterrupt::Device(_) => {}
        crate::platform::ClaimedInterrupt::Software(_) => {}
        crate::platform::ClaimedInterrupt::Spurious => return,
    }
    // Device handler 已由 controller owner 在 claim 内完成；opaque token 必须 exactly once
    // 返回同一 controller。RISC-V PLIC batch 已完成，静态 façade 在此消费 no-op token。
    crate::platform::complete_interrupt(claimed);
    if software {
        // 必须先 EOI/清除 local pending edge，再读取 barrier request；若反序，远端在
        // completion 与 EOI 之间发布的新 request 可能合并到旧 edge 并永久等待。
        crate::task::complete_pending_memory_barrier();
    }
}

pub(crate) fn init() {
    arch::trap::install_kernel_entry();
}

pub(crate) fn handle_user_trap() -> ! {
    arch::trap::install_kernel_entry();

    match arch::trap::event() {
        TrapEvent::TimerInterrupt => {
            // 仅重置下一次中断并发布 per-CPU deferred work，不在 hardirq 调度。
            timer::set_next_timer_interrupt();
            cpu::raise_deferred(DeferredWork::Timer);
        }
        TrapEvent::ExternalInterrupt => {
            handle_claimed_interrupt();
            if drivers::console_input_ready() {
                cpu::raise_deferred(DeferredWork::Console);
            }
        }
        TrapEvent::SoftwareInterrupt => {
            // RISC-V local SSIP 不经过 PLIC claim，仍由唯一 clear-then-barrier seam 确认。
            handle_supervisor_soft_interrupt();
        }
        TrapEvent::UnsupportedInterrupt => panic!("unsupported user interrupt"),
        TrapEvent::IllegalInstruction => {
            if let Some(current) = task::current_task() {
                match current.handle_illegal_instruction() {
                    Ok(()) => {
                        // PC 保持不变；return path 使用初始化后的 architecture state 重试原指令。
                    }
                    Err(fault) => {
                        current
                            .queue_synchronous_fault(
                                4,
                                task::PendingSignal::synchronous_fault(1, fault.address()),
                            )
                            .expect("SIGILL synchronous delivery must accept a valid current task");
                    }
                }
            } else {
                error!("[kernel] IllegalInstruction with no current task");
                exit_current_group_by_signal(4);
            }
        }
        TrapEvent::Breakpoint => {
            // 在尚未实现标准 SIGTRAP frame 前不能猜测 16/32-bit 指令长度并跳过断点。
            // 该 trap 完全由用户输入产生，只终止当前任务，不得 panic kernel。
            error!("[kernel] breakpoint in application, terminating current task");
            exit_current_group_by_signal(5);
        }
        TrapEvent::UserEnvironmentCall => {
            if let Some(current) = task::current_task() {
                // 1. transaction 只读取 a7/a0..a5/sepc 并原地推进 PC；不允许 context
                // 引用跨 syscall，因为 execve 会把同一 owner rebind 到新 AddressSpace。
                let (syscall_id, args, ecall_pc) = current.take_syscall_request();
                // sys_exit 不返回；若保留该 Arc，它会永久留在即将释放的 task stack 上。
                drop(current);
                let result = syscall::syscall(syscall_id, args);
                let current =
                    task::current_task().expect("returning syscall must still have a current task");

                // 2. execve 成功时，新 UserContext 已包含新程序入口；覆盖它会让 PC 回到旧映像。
                match result {
                    SyscallOutcome::Return(result) => {
                        if syscall_id != SYSCALL_EXECVE || result != 0 {
                            current.complete_syscall(SyscallCompletion::Return(result));
                        }
                    }
                    SyscallOutcome::Restart => {
                        current.complete_syscall(SyscallCompletion::Interrupted(
                            crate::syscall::INTERRUPTED_RESULT,
                        ));
                        current.arm_syscall_restart(syscall_id, args, ecall_pc);
                    }
                }
            } else {
                error!("[kernel] UserEnvCall with no current task, terminating");
                panic!("UserEnvCall with no current task");
            }
        }
        TrapEvent::InstructionPageFault { address } => {
            handle_user_page_fault(address, crate::memory::PageFaultAccess::Execute);
        }
        TrapEvent::StorePageFault { address } => {
            handle_user_page_fault(address, crate::memory::PageFaultAccess::Write);
        }
        TrapEvent::LoadPageFault { address } => {
            handle_user_page_fault(address, crate::memory::PageFaultAccess::Read);
        }
        TrapEvent::LoadAccessFault { address } | TrapEvent::StoreAccessFault { address } => {
            if let Some(current) = task::current_task() {
                let program_counter = current.user_program_counter();
                error!(
                    "[kernel] access fault in application, bad addr = {address:#x}, pc = {program_counter:#x}, core dumped.",
                );
            } else {
                error!(
                    "[kernel] access fault with no current task, bad addr = {address:#x}, core dumped.",
                );
            }
            exit_current_group_by_signal(11);
        }
        TrapEvent::UnsupportedException { address } => {
            error!("[kernel] unsupported application exception, fault address={address:#x}");
            exit_current_group_by_signal(4);
        }
    }

    // 所有 user trap 在领域 handler 已释放 syscall/page-table/driver lock 后汇入唯一
    // deferred safe point。缺失此处会让 kernel 中确认过的 SSIP 永久留下 pending work；
    // 若把它移回 kernel-trap arm，则普通 VirtIO queue lock 会发生同 CPU 重入死锁。
    task::dispatch_pending_deferred_work();

    // kernel/user timer softirq 共用该 flag；只在即将返回用户态时切换，避免在 hardirq 中调度。
    if task::take_reschedule() && task::current_task().is_some() {
        task::suspend_current_and_run_next();
    }
    trap_return();
}

fn handle_user_page_fault(address: usize, access: crate::memory::PageFaultAccess) {
    let outcome = task::current_task().map(|current| current.handle_page_fault(address, access));
    match outcome {
        Some(Ok(crate::memory::PageFaultOutcome::Handled)) => {}
        Some(Ok(crate::memory::PageFaultOutcome::BusError)) => {
            debug!("shared file mapping beyond EOF, VA:{address:#x}");
            exit_current_group_by_signal(7);
        }
        // 物理页耗尽不是 address violation；缺少该分支会把真实 OOM 静默伪装为 SIGSEGV，
        // 让 userspace 无法区分坏指针与无 swap 系统的 memory-pressure termination。
        Some(Err(error)) if error.is_out_of_memory() => {
            debug!("user page fault out of memory, VA:{address:#x}");
            exit_current_group_by_signal(9);
        }
        Some(Ok(crate::memory::PageFaultOutcome::SegmentationFault)) | Some(Err(_)) | None => {
            debug!("user page fault, VA:{address:#x}");
            exit_current_group_by_signal(11);
        }
    }
}

pub(crate) fn trap_return() -> ! {
    // 1. 后续 address-space/context snapshot 必须不被 local scheduling 打断；该路径 noreturn，
    // 因此 previous interrupt state 不得由 Rust frame 恢复。
    arch::interrupt::disable_for_transfer();

    // 2. signal preparation 每轮只保活当前 TCB，stop/terminate 会释放它再进入 scheduler。
    loop {
        task::exit_current_if_group_exiting();
        let delivery_task = crate::task::current_task().expect("No current task in trap_return");
        match delivery_task.prepare_signal_delivery() {
            Ok(SignalDelivery::None) => break,
            Ok(SignalDelivery::Stop(signal)) => {
                drop(delivery_task);
                stop_current_process(signal);
            }
            Ok(SignalDelivery::Terminate(signal)) => {
                drop(delivery_task);
                exit_current_group_by_signal(signal);
            }
            Err(_) => {
                drop(delivery_task);
                exit_current_group_by_signal(11);
            }
        }
    }
    let current_task = crate::task::current_task().expect("signal delivery lost current task");
    let user_address_space = current_task.user_token();
    let user_context_va = current_task.prepare_user_return(crate::cpu::current_id().index());
    // 3. trap return 通过 noreturn trampoline 跳转，Rust frame 不会展开；若不在此显式释放，
    // 每次 syscall 都会把一个 TCB Arc 永久遗留在随后被覆盖的 task kernel stack 上。
    drop(current_task);

    arch::trap::return_to_user(user_context_va, user_address_space, TRAMPOLINE)
}

pub(crate) fn handle_kernel_trap() {
    match arch::trap::event() {
        TrapEvent::TimerInterrupt => {
            timer::set_next_timer_interrupt();
            // kernel/user timer 使用同一 per-CPU softirq；hardirq 不扫描任务表或分配。
            cpu::raise_deferred(DeferredWork::Timer);
        }
        TrapEvent::ExternalInterrupt => {
            // 内核态同步 I/O 可以被 external IRQ 打断；此处只确认 platform
            // interrupt-controller 状态，不在 hardirq 中调度。
            handle_claimed_interrupt();
            if drivers::console_input_ready() {
                cpu::raise_deferred(DeferredWork::Console);
            }
        }
        TrapEvent::SoftwareInterrupt => {
            handle_supervisor_soft_interrupt();
        }
        event => panic!("kernel trap: {:?}", arch::trap::kernel_exception(event)),
    }
}
