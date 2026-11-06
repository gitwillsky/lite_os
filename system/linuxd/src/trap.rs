//! Linux 线程的承载线程主循环。
//!
//! # 循环
//!
//! 1. 返回用户态前的安全点：group exit、group stop、信号投递，以及 exec 后的承载线程交接；
//! 2. 写回寄存器并进入 restricted mode；
//! 3. 按退出原因分派：syscall 交给 Linux syscall 分派；异常先按 VMA 分类（栈增长、SIGSEGV、SIGBUS），
//!    其余异常转换为同步信号；kick 只为让线程回到安全点观察信号或停止请求。
//!
//! # 引用
//!
//! 承载线程的上下文字持有执行体的一个强引用；循环体内的局部引用在调用任何不返回的退出函数之前
//! 必须释放，否则执行体永远不会被回收。

use alloc::sync::Arc;

use linux_abi::signal::{
    BUS_ADRALN, BUS_ADRERR, ILL_ILLOPC, ILL_ILLTRP, SEGV_ACCERR, SEGV_MAPERR, SIGBUS, SIGILL,
    SIGKILL, SIGSEGV, SIGTRAP, TRAP_BRKPT,
};
use native_runtime::Process;
use native_runtime::native_abi::{ExceptionReport, exception};
use native_runtime::restricted::{BoundState, Exit};
use native_runtime::thread::PreparedThread;

use crate::arch::context::{SyscallCompletion, UserFault};
use crate::memory::{MemoryError, PageFaultAccess, PageFaultOutcome, SegmentationCause};
use crate::syscall::{self, SyscallOutcome};
use crate::task::{
    self, SignalDelivery, TaskControlBlock, current_task, exit_current_group_by_signal,
    stop_current_process,
};

/// 承载线程的栈大小（linuxd 处理 syscall 的最深调用链在这里运行）。
pub(crate) const CARRIER_STACK: usize = 256 * 1024;

/// 在 `process`（Linux 线程所在地址空间的成员进程）中准备用户线程的承载线程。
///
/// # Errors
///
/// 线程或栈创建失败。
pub(crate) fn prepare_user_carrier(
    process: &Arc<Process>,
) -> Result<PreparedThread<()>, MemoryError> {
    native_runtime::thread::prepare_in(process, b"linux", CARRIER_STACK, run_user)
        .map_err(MemoryError::from_status)
}

/// 在 linuxd 自身进程中准备内核线程的承载线程。
///
/// # Errors
///
/// 线程或栈创建失败。
pub(crate) fn prepare_kernel_carrier(
    name: &'static str,
) -> Result<PreparedThread<()>, MemoryError> {
    native_runtime::thread::prepare_in(
        native_runtime::self_process(),
        name.as_bytes(),
        CARRIER_STACK,
        run_kernel,
    )
    .map_err(MemoryError::from_status)
}

fn run_kernel() {
    let body = current_task()
        .expect("kernel carrier started without a task")
        .take_kernel_thread_body();
    body();
    task::exit_current_kernel_thread()
}

fn run_user() {
    let task = current_task().expect("user carrier started without a task");
    crate::arch::user_copy::register().expect("carrier user copy recovery");
    if let Some(fp) = task.take_initial_fp() {
        crate::arch::fp::restore(&fp);
    }
    match BoundState::bind(native_runtime::heap::vmar()) {
        Ok(state) => task.install_restricted_state(state),
        Err(_) => {
            // 没有 restricted 状态线程无法运行用户代码：与 Linux 内核栈分配失败后的强制退出一致。
            drop(task);
            exit_current_group_by_signal(SIGKILL)
        }
    }
    drop(task);
    loop {
        return_to_user();
        let task = current_task().expect("carrier lost its task");
        let result = task.enter_restricted();
        let (exit, report) =
            result.unwrap_or_else(|status| panic!("restricted entry rejected: {status}"));
        match exit {
            Exit::Syscall => handle_syscall(task),
            Exit::Exception => handle_exception(task, report),
            Exit::Kick => drop(task),
        }
    }
}

fn handle_syscall(current: Arc<TaskControlBlock>) {
    let (syscall_id, args, syscall_pc) = current.take_syscall_request();
    let result = syscall::syscall(syscall_id, args, current);
    let current = current_task().expect("returning syscall must still have a current task");
    match result {
        SyscallOutcome::PreserveRegisters => {}
        SyscallOutcome::Return(result) => {
            current.complete_syscall(SyscallCompletion::Return(result))
        }
        SyscallOutcome::Restart(policy) => {
            current.complete_syscall(SyscallCompletion::Interrupted(
                crate::syscall::INTERRUPTED_RESULT,
            ));
            current.arm_syscall_restart(syscall_id, args, syscall_pc, policy);
        }
    }
}

fn handle_exception(current: Arc<TaskControlBlock>, report: ExceptionReport) {
    let fault = UserFault::new(report);
    match report.kind {
        exception::PAGE_FAULT => {
            let access = match report.access {
                1 => PageFaultAccess::Write,
                2 => PageFaultAccess::Execute,
                _ => PageFaultAccess::Read,
            };
            handle_user_page_fault(current, fault, access);
        }
        exception::ACCESS_FAULT => force_memory_fault(&current, SIGSEGV, SEGV_ACCERR, fault),
        exception::ALIGNMENT => force_memory_fault(&current, SIGBUS, BUS_ADRALN, fault),
        exception::ILLEGAL_INSTRUCTION => {
            force_synchronous_fault(&current, SIGILL, ILL_ILLOPC, fault.address())
        }
        exception::BREAKPOINT => {
            let address = current.user_program_counter();
            force_synchronous_fault(&current, SIGTRAP, TRAP_BRKPT, address);
        }
        _ => force_synchronous_fault(&current, SIGILL, ILL_ILLTRP, fault.address()),
    }
}

fn force_synchronous_fault(current: &TaskControlBlock, signal: usize, code: i32, address: usize) {
    current
        .queue_synchronous_fault(
            signal,
            task::PendingSignal::synchronous_fault(code, address),
        )
        .expect("synchronous fault delivery must accept a valid current task");
}

fn force_memory_fault(current: &TaskControlBlock, signal: usize, code: i32, fault: UserFault) {
    current.record_user_fault(fault);
    force_synchronous_fault(current, signal, code, fault.address());
}

fn handle_user_page_fault(
    current: Arc<TaskControlBlock>,
    fault: UserFault,
    access: PageFaultAccess,
) {
    match current.handle_page_fault(fault.address(), access) {
        Ok(PageFaultOutcome::Handled) => {}
        Ok(PageFaultOutcome::BusError) | Err(MemoryError::Io) => {
            force_memory_fault(&current, SIGBUS, BUS_ADRERR, fault);
        }
        Err(error) if error.is_out_of_memory() => {
            debug!("user page fault out of memory, VA:{:#x}", fault.address());
            drop(current);
            exit_current_group_by_signal(SIGKILL);
        }
        Ok(PageFaultOutcome::SegmentationFault(SegmentationCause::AccessDenied)) => {
            force_memory_fault(&current, SIGSEGV, SEGV_ACCERR, fault);
        }
        Ok(PageFaultOutcome::SegmentationFault(SegmentationCause::Unmapped)) | Err(_) => {
            force_memory_fault(&current, SIGSEGV, SEGV_MAPERR, fault);
        }
    }
}

/// 返回用户态前的安全点；不返回的分支（退出、交接）结束本承载线程。
fn return_to_user() {
    loop {
        task::exit_current_if_group_exiting();
        let current = current_task().expect("no current task at user return");
        task::stop_point(&current);
        match current.prepare_signal_delivery() {
            Ok(SignalDelivery::None) => {}
            Ok(SignalDelivery::Stop(signal)) => {
                drop(current);
                stop_current_process(signal);
                continue;
            }
            Ok(SignalDelivery::Terminate(signal)) => {
                drop(current);
                exit_current_group_by_signal(signal);
            }
            Err(_) => {
                drop(current);
                exit_current_group_by_signal(SIGSEGV);
            }
        }
        if let Some(successor) = current.take_prepared_carrier() {
            drop(current);
            hand_off(successor);
        }
        return;
    }
}

/// exec 后的承载线程交接：执行体的地址空间已是另一个成员进程，换由其中准备好的承载线程运行。
fn hand_off(successor: PreparedThread<()>) -> ! {
    let task = task::take_current_task().expect("hand-off without a current task");
    let descriptor = successor.descriptor().clone();
    descriptor.set_context(Arc::into_raw(task.clone()) as usize);
    drop(task.set_carrier(Some(descriptor)));
    task::apply_scheduling(&task);
    drop(task);
    drop(successor.start().expect("successor carrier start failed"));
    native_runtime::task::exit_thread()
}
