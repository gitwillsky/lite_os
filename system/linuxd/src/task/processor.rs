//! Linux 执行体与承载它的 native 线程。
//!
//! # 当前执行体
//!
//! 承载线程描述符的上下文字保存 `Arc<TaskControlBlock>` 的原始指针（一个强引用）。服务自身的线程
//! （pager、中断、deferred）上下文字为零：没有当前 Linux 执行体。
//!
//! # 阻塞与唤醒
//!
//! 1. 阻塞者在调度状态中发布精确的 [`WaitMembership`] 并转为 `Blocked`，然后在承载线程的 parker 上停驻；
//! 2. 唤醒者在同一把锁内比较并消费 membership、写入结果、转回 `Running`，锁外 unpark 承载线程；
//! 3. 停驻者醒来复查：membership 已消费且没有停止请求时返回结果，否则继续停驻。
//!
//! # 停止
//!
//! 运行中的线程被标为 `StopPending` 并被 kick（离开 restricted mode），在返回用户态前的安全点停驻；
//! 阻塞中的线程被标为 `Stopped { resume: Blocked }`，被唤醒后仍保持停止，直到 SIGCONT。

use alloc::sync::Arc;
use alloc::vec::Vec;

use native_runtime::thread::Park;

use super::{RunState, StopResume, TaskControlBlock, WaitMembership};
use crate::sync::WaitResult;

/// 当前承载线程上的 Linux 执行体。
pub(crate) fn current_task() -> Option<Arc<TaskControlBlock>> {
    let raw = native_runtime::thread::current().context() as *const TaskControlBlock;
    if raw.is_null() {
        return None;
    }
    // SAFETY: 上下文字由 `install_current` 以 `Arc::into_raw` 写入并持有一个强引用，
    // 只有本线程的 `take_current_task` 会收回它，因此对象此刻存活。
    unsafe {
        Arc::increment_strong_count(raw);
        Some(Arc::from_raw(raw))
    }
}

/// 取回当前执行体（承载线程即将退出或交接）。
pub(crate) fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    let descriptor = native_runtime::thread::current();
    let raw = descriptor.context() as *const TaskControlBlock;
    if raw.is_null() {
        return None;
    }
    descriptor.set_context(0);
    // SAFETY: 收回 `install_current` 交出的那个强引用。
    Some(unsafe { Arc::from_raw(raw) })
}

/// 消费精确等待身份并使线程可继续。
///
/// # Returns
///
/// 身份匹配并被本次调用消费时为 `true`；重复或过期的唤醒为 `false`。
pub(in crate::task) fn wake_waiting_task(
    task: Arc<TaskControlBlock>,
    expected: WaitMembership,
    result: Option<WaitResult>,
) -> bool {
    {
        let mut scheduling = task.scheduling.state.lock();
        if scheduling.wait != Some(expected) {
            return false;
        }
        scheduling.wait = None;
        scheduling.wait_result = result;
        match scheduling.run_state() {
            RunState::Blocked => scheduling.set_run_state(RunState::Running),
            RunState::Stopped {
                resume: StopResume::Blocked,
            } => scheduling.set_run_state(RunState::Stopped {
                resume: StopResume::Runnable,
            }),
            // 尚未停驻（发布身份后、停驻前）或正在退出：停驻者复查时看到已消费的身份。
            RunState::Running | RunState::StopPending | RunState::Exited => {}
            state => panic!("wait membership attached to invalid state {state:?}"),
        }
    }
    if let Some(carrier) = task.carrier() {
        carrier.unpark();
    }
    true
}

/// 同 [`wake_waiting_task`]；CPU 局部性由内核调度器决定，这里没有额外提示。
pub(in crate::task) fn wake_waiting_task_on_current_cpu(
    task: Arc<TaskControlBlock>,
    expected: WaitMembership,
    result: Option<WaitResult>,
) -> bool {
    wake_waiting_task(task, expected, result)
}

pub(super) fn wake_deadline_task(task: Arc<TaskControlBlock>, wait_id: u64, result: WaitResult) -> bool {
    wake_waiting_task(task, WaitMembership::Deadline(wait_id), Some(result))
}

pub(super) fn wake_child_task(task: Arc<TaskControlBlock>, result: WaitResult) -> bool {
    wake_waiting_task(task, WaitMembership::Child, Some(result))
}

pub(super) fn wake_futex_task(task: Arc<TaskControlBlock>, wait_id: u64, result: WaitResult) -> bool {
    wake_waiting_task(task, WaitMembership::Futex(wait_id), Some(result))
}

pub(super) fn wake_console_task(task: Arc<TaskControlBlock>, wait_id: u64, result: WaitResult) -> bool {
    wake_waiting_task(task, WaitMembership::Console(wait_id), Some(result))
}

pub(super) fn wake_signal_task(task: Arc<TaskControlBlock>, wait_id: u64, result: WaitResult) -> bool {
    wake_waiting_task(task, WaitMembership::Signal(wait_id), Some(result))
}

pub(super) fn wake_pipe_task(task: Arc<TaskControlBlock>, wait_id: u64, result: WaitResult) -> bool {
    wake_waiting_task(task, WaitMembership::Pipe(wait_id), Some(result))
}

pub(super) fn wake_flock_task(task: Arc<TaskControlBlock>, wait_id: u64, result: WaitResult) -> bool {
    wake_waiting_task(task, WaitMembership::AdvisoryLock(wait_id), Some(result))
}

pub(super) fn wake_poll_task(task: Arc<TaskControlBlock>, wait_id: u64, result: WaitResult) -> bool {
    wake_waiting_task(task, WaitMembership::Poll(wait_id), Some(result))
}

/// 已发布等待身份、尚未停驻的一次阻塞。
pub(in crate::task) struct PreparedBlock {
    task: Option<Arc<TaskControlBlock>>,
}

impl PreparedBlock {
    /// 停驻直到身份被消费且没有停止请求，返回唤醒结果。
    pub(in crate::task) fn suspend(mut self) -> WaitResult {
        let task = self.task.take().expect("prepared block consumed twice");
        let carrier = task.carrier().expect("blocking task has no carrier thread");
        loop {
            {
                let mut scheduling = task.scheduling.state.lock();
                if scheduling.wait.is_none() && scheduling.run_state() == RunState::Running {
                    return scheduling.wait_result.take().unwrap_or(WaitResult::Woken);
                }
            }
            while carrier.park(native_runtime::native_abi::DEADLINE_INFINITE) != Park::Unparked {}
        }
    }
}

impl Drop for PreparedBlock {
    fn drop(&mut self) {
        assert!(self.task.is_none(), "prepared block dropped before suspend");
    }
}

/// 把当前线程转为阻塞并发布等待身份；`owner` 覆盖“复查条件 → 发布身份”，返回前释放。
pub(in crate::task) fn prepare_current_block<Owner>(
    task: &Arc<TaskControlBlock>,
    mut owner: Owner,
    publish: impl FnOnce(&mut Owner, Arc<TaskControlBlock>) -> WaitMembership,
) -> PreparedBlock {
    task.scheduling
        .last_cpu
        .store(crate::cpu::current_id().index(), core::sync::atomic::Ordering::Relaxed);
    {
        let mut scheduling = task.scheduling.state.lock();
        assert!(scheduling.wait.is_none(), "task blocks twice");
        assert!(scheduling.wait_result.is_none(), "stale wake result before blocking");
        scheduling.wait = Some(publish(&mut owner, task.clone()));
        match scheduling.run_state() {
            RunState::Running => scheduling.set_run_state(RunState::Blocked),
            RunState::StopPending => scheduling.set_run_state(RunState::Stopped {
                resume: StopResume::Blocked,
            }),
            state => panic!("blocking from invalid state {state:?}"),
        }
    }
    // 跨停驻持有外部锁会让唤醒者永久等待。
    drop(owner);
    PreparedBlock {
        task: Some(task.clone()),
    }
}

/// 返回用户态前的停止安全点：有停止请求时停驻到 SIGCONT（或 tracer 恢复）。
///
/// 1. `StopPending` 在这里转为 `Stopped`，随后交付 ptrace/group stop 的完成通知；
/// 2. 停止状态下启动的新线程（`Stopped { New }`）在第一个安全点转为可恢复的停止，不再重复通知；
/// 3. 停驻直到状态离开 `Stopped`。
pub(crate) fn stop_point(task: &TaskControlBlock) {
    let transitioned = {
        let mut scheduling = task.scheduling.state.lock();
        let transitioned = match scheduling.run_state() {
            RunState::StopPending => true,
            RunState::Stopped {
                resume: StopResume::New,
            } => false,
            _ => return,
        };
        scheduling.set_run_state(RunState::Stopped {
            resume: StopResume::Runnable,
        });
        transitioned
    };
    if transitioned {
        super::process_table::complete_stop_transition(task);
    }
    let carrier = task.carrier().expect("stopping task has no carrier thread");
    loop {
        if !matches!(task.scheduling.state.lock().run_state(), RunState::Stopped { .. }) {
            return;
        }
        while carrier.park(native_runtime::native_abi::DEADLINE_INFINITE) != Park::Unparked {}
    }
}

/// 要求线程尽快到达安全点（信号投递）：在 restricted mode 中则立即退出。
pub(in crate::task) fn request_task_reschedule(task: &Arc<TaskControlBlock>) {
    if let Some(carrier) = task.carrier() {
        let _ = carrier.handle().kick();
    }
}

/// group stop。
pub(in crate::task) fn request_task_stop(task: &Arc<TaskControlBlock>) {
    let kick = {
        let mut scheduling = task.scheduling.state.lock();
        match scheduling.run_state() {
            RunState::New => {
                scheduling.set_run_state(RunState::Stopped {
                    resume: StopResume::New,
                });
                false
            }
            RunState::Running => {
                scheduling.set_run_state(RunState::StopPending);
                true
            }
            RunState::Blocked => {
                scheduling.set_run_state(RunState::Stopped {
                    resume: StopResume::Blocked,
                });
                false
            }
            RunState::StopPending | RunState::Stopped { .. } | RunState::Exited => false,
        }
    };
    if kick {
        request_task_reschedule(task);
    }
}

/// 取消 group stop，恢复原有状态。
pub(in crate::task) fn continue_stopped_task(task: Arc<TaskControlBlock>) {
    {
        let mut scheduling = task.scheduling.state.lock();
        let next = match scheduling.run_state() {
            // 承载线程已启动（执行体已入队）时由它的安全点继续运行；否则仍待入队。
            RunState::Stopped {
                resume: StopResume::New,
            } if task.carrier().is_some() => RunState::Running,
            RunState::Stopped {
                resume: StopResume::New,
            } => RunState::New,
            RunState::Stopped {
                resume: StopResume::Runnable,
            }
            | RunState::StopPending => RunState::Running,
            RunState::Stopped {
                resume: StopResume::Blocked,
            } => RunState::Blocked,
            _ => return,
        };
        scheduling.set_run_state(next);
    }
    if let Some(carrier) = task.carrier() {
        carrier.unpark();
    }
}

/// 可运行线程数（`procs_running`）。
pub(crate) fn runnable_task_count() -> usize {
    super::model::runnable_task_count()
}

/// 每个 CPU 的累计忙碌微秒（`/proc/stat`）。
///
/// # Errors
///
/// 内核信息读取失败或内存不足。
pub(crate) fn cpu_runtime_snapshot() -> Result<Vec<(usize, u64)>, ()> {
    let count = crate::cpu::count();
    let mut busy = alloc::vec![0u64; count];
    let bytes = core::mem::size_of_val(busy.as_slice());
    // SAFETY: `u64` 数组的字节视图，长度与对齐都正确。
    let buffer = unsafe { core::slice::from_raw_parts_mut(busy.as_mut_ptr().cast::<u8>(), bytes) };
    native_runtime::system::get_info(native_runtime::native_abi::system::CPU_BUSY, buffer).map_err(|_| ())?;
    Ok(busy.into_iter().enumerate().map(|(cpu, ns)| (cpu, ns / 1000)).collect())
}

/// 把线程的 nice 与亲和性下发到承载线程。
pub(crate) fn apply_scheduling(task: &TaskControlBlock) {
    let Some(carrier) = task.carrier() else {
        return;
    };
    let priority = task.scheduling.policy.lock().kernel_priority();
    let affinity = task.scheduling.state.lock().cpu_affinity.effective_bits();
    let _ = carrier.handle().set_priority(priority);
    let _ = carrier.handle().set_affinity(affinity);
}

/// 启动新执行体：把它装入已准备的承载线程并启动。
///
/// # Panics
///
/// 执行体没有已准备的承载线程，或承载线程所属进程已终止（地址空间已被销毁，不变量破坏）。
pub(in crate::task) fn enqueue_new_task(task: Arc<TaskControlBlock>) {
    {
        static DIAG_COUNT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
        let n = DIAG_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if n % 128 == 0 {
            let blocks = native_runtime::system::free_blocks().unwrap_or_default();
            let free: u64 = blocks.iter().enumerate().map(|(order, count)| count << order).sum();
            error!("DIAG tasks={} free_pages={} basic={:?}", n, free, native_runtime::system::basic());
        }
    }
    let prepared = task
        .take_prepared_carrier()
        .expect("new task has no prepared carrier thread");
    let descriptor = prepared.descriptor().clone();
    descriptor.set_context(Arc::into_raw(task.clone()) as usize);
    task.set_carrier(Some(descriptor));
    {
        let mut scheduling = task.scheduling.state.lock();
        if scheduling.run_state() == RunState::New {
            scheduling.set_run_state(RunState::Running);
        }
    }
    apply_scheduling(&task);
    // 承载线程与执行体同寿命；分离后由运行时在其终止时回收栈。
    drop(prepared.start().expect("carrier thread start failed"));
}
