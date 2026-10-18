//! 陷入（trap）处理：从用户态进入内核、再返回用户态的完整路径。
//!
//! # 分工
//!
//! 汇编入口和寄存器布局属于 `arch`：它把原始异常解码成语义事件 `TrapEvent`。本模块只接收这些事件，
//! 分派给对应领域（syscall、内存、信号、调度），自己不理解寄存器。
//!
//! # 用户态 trap 的一条直线（`handle_user_trap`）
//!
//! 1. 重新安装内核态的 trap 入口。
//! 2. 取得事件并分派：
//!    - 系统调用 → `syscall::syscall`。返回值决定是写回结果、保持寄存器不变（exec 成功后新程序的
//!      入口已经写好），还是安排重启。
//!    - 缺页与访问错误 → 交给内存子系统，结果映射成信号：越过文件末尾或 I/O 失败是 SIGBUS；物理页耗尽
//!      是 SIGKILL，而不是伪装成 SIGSEGV，这样用户态能区分坏指针和内存压力；地址未映射和权限不足
//!      是带不同 `si_code` 的 SIGSEGV。
//!    - 非法指令、断点等同步异常 → 作为信号发给当前线程（`force_synchronous_fault`）。
//!    - 时钟、外设、核间中断 → 确认硬件，把真正的工作登记为 deferred work。
//! 3. **汇合点**：所有用户态 trap 在释放了各领域的锁之后，统一执行一次 `dispatch_pending_deferred_work`，
//!    把中断里登记的工作做掉。
//! 4. 若调度标志已置位，就在这里切换到下一个任务。抢占式切换只发生在即将返回用户态时，不在中断
//!    处理函数里。
//! 5. `trap_return`：循环处理待投递的信号（可能要往用户栈写 signal frame，也可能让进程停止或终止），
//!    最后关中断、激活地址空间，经 trampoline 跳回用户态，不再返回。
//!
//! # 内核态 trap
//!
//! `handle_kernel_trap` 只接受中断（时钟、外设、核间中断），同样只确认硬件并登记 deferred work，
//! 不调度。其他任何内核态异常都是内核自己的 bug，直接 panic（fail-stop）。
//!
//! # 中断确认的顺序是协议的一部分
//!
//! 时钟中断必须先重置时钟源再 EOI，反过来中断控制器会立刻重投同一个中断；核间中断必须先 EOI、清除
//! 本地 pending 的边沿，再读取所有请求，否则远端在这两步之间发布的新请求可能被合并进旧的边沿而
//! 永远等不到处理。
//!
//! # 阅读顺序
//!
//! `handle_user_trap` → `handle_user_page_fault` → `trap_return` → `handle_claimed_interrupt`。

use core::panic;

use syscall_abi::signal::{
    BUS_ADRERR, ILL_ILLOPC, ILL_ILLTRP, SEGV_ACCERR, SEGV_MAPERR, SIGBUS, SIGILL, SIGKILL, SIGSEGV,
    SIGTRAP, TRAP_BRKPT,
};

use crate::{
    arch::{self, context::SyscallCompletion, trap::TrapEvent},
    deferred::{self, DeferredWork},
    memory::TRAMPOLINE,
    memory::{MemoryError, PageFaultAccess, PageFaultOutcome, SegmentationCause},
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
            deferred::raise(DeferredWork::TIMER);
        }
        crate::platform::ClaimedInterrupt::Device(_) => {}
        crate::platform::ClaimedInterrupt::Software(_) => {}
        crate::platform::ClaimedInterrupt::Spurious => return,
    }
    // Device handler 已由 controller owner 在 claim 内完成；opaque token 必须 exactly once
    // 返回同一 controller。RISC-V PLIC batch 已完成，静态 façade 在此消费 no-op token。
    crate::platform::complete_interrupt(claimed);
    if software {
        // 必须先 EOI/清除 local pending edge，再读取所有 SGI request；若反序，远端在
        // completion 与 EOI 之间发布的新 generation 可能合并到旧 edge 并永久等待。
        crate::platform::complete_pending_ipi();
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
            deferred::raise(DeferredWork::TIMER);
        }
        TrapEvent::ExternalInterrupt => {
            handle_claimed_interrupt();
            if crate::hal::console::input_ready() {
                deferred::raise(DeferredWork::CONSOLE);
            }
        }
        TrapEvent::SoftwareInterrupt => {
            // RISC-V local SSIP 不经过 PLIC claim，仍由唯一 clear-then-barrier seam 确认。
            handle_supervisor_soft_interrupt();
        }
        TrapEvent::UnsupportedInterrupt => panic!("unsupported user interrupt"),
        TrapEvent::IllegalInstruction => {
            let current = task::current_task().expect("user illegal instruction without a task");
            // Ok 时 PC 保持不变；return path 使用初始化后的 architecture state 重试原指令。
            if let Err(fault) = current.handle_illegal_instruction() {
                force_synchronous_fault(&current, SIGILL, ILL_ILLOPC, fault.address());
            }
        }
        TrapEvent::Breakpoint => {
            let current = task::current_task().expect("user breakpoint without a task");
            let address = current.user_program_counter();
            force_synchronous_fault(&current, SIGTRAP, TRAP_BRKPT, address);
        }
        TrapEvent::UserEnvironmentCall => {
            if let Some(current) = task::current_task() {
                // 1. transaction 只读取 syscall number/argument register 与 syscall PC 并原地推进 PC；不允许 context
                // 引用跨 syscall，因为 execve 会把同一 owner rebind 到新 AddressSpace。
                let (syscall_id, args, syscall_pc) = current.take_syscall_request();
                // 把已有 Arc 转交 seccomp，避免 disabled 路径重复 current lookup/Arc clone；
                // dispatcher 在任何不返回的 handler 前释放它，不能留在退休 task stack 上。
                let result = syscall::syscall(syscall_id, args, current);
                let current =
                    task::current_task().expect("returning syscall must still have a current task");

                // 2. execve 成功时，新 UserContext 已包含新程序入口；覆盖它会让 PC 回到旧映像。
                match result {
                    SyscallOutcome::PreserveRegisters => {}
                    SyscallOutcome::Return(result) => {
                        current.complete_syscall(SyscallCompletion::Return(result));
                    }
                    SyscallOutcome::Restart(policy) => {
                        current.complete_syscall(SyscallCompletion::Interrupted(
                            crate::syscall::INTERRUPTED_RESULT,
                        ));
                        current.arm_syscall_restart(syscall_id, args, syscall_pc, policy);
                    }
                }
            } else {
                panic!("user syscall without a current task");
            }
        }
        TrapEvent::InstructionPageFault { address } => {
            handle_user_page_fault(address, PageFaultAccess::Execute);
        }
        TrapEvent::StorePageFault { address } => {
            handle_user_page_fault(address, PageFaultAccess::Write);
        }
        TrapEvent::LoadPageFault { address } => {
            handle_user_page_fault(address, PageFaultAccess::Read);
        }
        TrapEvent::LoadAccessFault { address } | TrapEvent::StoreAccessFault { address } => {
            let current = task::current_task().expect("user access fault without a task");
            force_synchronous_fault(&current, SIGSEGV, SEGV_ACCERR, address);
        }
        TrapEvent::UnsupportedException { address } => {
            let current = task::current_task().expect("user exception without a task");
            force_synchronous_fault(&current, SIGILL, ILL_ILLTRP, address);
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

/// 按 Linux `force_sig_fault` 语义向当前 Thread 发布同步 fault signal。
///
/// caught 且未屏蔽的 handler 保持不变；blocked 或 `SIG_IGN` 在同一事务中恢复默认并解除屏蔽，
/// 使 handler 能处理 guard page、feature probe 等可恢复 fault，而默认 disposition 仍终止进程。
///
/// # Parameters
///
/// - `current`: 触发 trap 的当前 Thread。
/// - `signal`: 同步 fault signal number。
/// - `code`: signal-specific 正 `si_code`。
/// - `address`: 写入 `si_addr` 的 fault 地址。
///
/// # Panics
///
/// 当前 Thread 拒绝合法 forced fault 时视为 signal owner 不变量破坏并 fail-stop。
fn force_synchronous_fault(
    current: &task::TaskControlBlock,
    signal: usize,
    code: i32,
    address: usize,
) {
    current
        .queue_synchronous_fault(
            signal,
            task::PendingSignal::synchronous_fault(code, address),
        )
        .expect("synchronous fault delivery must accept a valid current task");
}

fn handle_user_page_fault(address: usize, access: PageFaultAccess) {
    let current = task::current_task().expect("user page fault without a task");
    match current.handle_page_fault(address, access) {
        Ok(PageFaultOutcome::Handled) => {}
        // file mapping 越过 EOF 与 backing I/O 失败均是 Linux `VM_FAULT_SIGBUS`。
        Ok(PageFaultOutcome::BusError) | Err(MemoryError::Io) => {
            force_synchronous_fault(&current, SIGBUS, BUS_ADRERR, address);
        }
        // 物理页耗尽不是 address violation；缺少该分支会把真实 OOM 伪装为可捕获的 SIGSEGV，
        // 让 userspace 无法区分坏指针与无 swap 系统的 memory-pressure termination。
        Err(error) if error.is_out_of_memory() => {
            debug!("user page fault out of memory, VA:{address:#x}");
            // exit 不返回；必须先释放当前 TCB Arc，避免其永久留在即将释放的 task stack 上。
            drop(current);
            exit_current_group_by_signal(SIGKILL);
        }
        Ok(PageFaultOutcome::SegmentationFault(SegmentationCause::AccessDenied)) => {
            force_synchronous_fault(&current, SIGSEGV, SEGV_ACCERR, address);
        }
        Ok(PageFaultOutcome::SegmentationFault(SegmentationCause::Unmapped)) | Err(_) => {
            force_synchronous_fault(&current, SIGSEGV, SEGV_MAPERR, address);
        }
    }
}

pub(crate) fn trap_return() -> ! {
    // 1. signal preparation 可 fault 或等待 mm；每轮只保活当前 TCB，stop/terminate
    // 释放它再进入 scheduler。当前 Thread 的 scheduler owner 保证 context 不被并发执行；
    // kernel hardirq 只发布 deferred/ack，不修改 UserContext 或调度。
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
                // signal frame 无法写入用户栈：按 Linux `force_sigsegv` 以 SIGSEGV 终止。
                drop(delivery_task);
                exit_current_group_by_signal(SIGSEGV);
            }
        }
    }
    // 2. 在全部可阻塞 preparation 之后建立最终 transfer window。TaskMutex wait 的
    // context-switch handoff 可恢复 outgoing task 的 IRQ 状态，不能沿用 loop 之前的屏蔽。
    // 下面只做非阻塞 snapshot/ASID activation，noreturn frame 不得恢复 previous IRQ state。
    arch::interrupt::disable_for_transfer();
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
            deferred::raise(DeferredWork::TIMER);
        }
        TrapEvent::ExternalInterrupt => {
            // 内核态同步 I/O 可以被 external IRQ 打断；此处只确认 platform
            // interrupt-controller 状态，不在 hardirq 中调度。
            handle_claimed_interrupt();
            if crate::hal::console::input_ready() {
                deferred::raise(DeferredWork::CONSOLE);
            }
        }
        TrapEvent::SoftwareInterrupt => {
            handle_supervisor_soft_interrupt();
        }
        event => panic!("kernel trap: {:?}", arch::trap::kernel_exception(event)),
    }
}
