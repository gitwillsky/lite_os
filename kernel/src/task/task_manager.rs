use core::sync::atomic::Ordering;

use alloc::{collections::BTreeMap, string::ToString, sync::Arc};
use lazy_static::lazy_static;

use crate::{
    arch::hart::hart_id,
    sync::IrqRwLock,
    task::{
        self, Processor, TaskControlBlock, TaskStatus, context::TaskContext,
        processor::add_task_to_best_cpu, with_current_processor,
    },
    timer::{get_time_ns, get_time_us},
};

/// 系统中存活 Process 的 TGID 索引。
///
/// 该表只证明 task 存在，不是调度状态权威；runqueue/current/status 的统一事务留给 Phase 6。
pub struct TaskManager {
    /// 全局进程表：TGID -> 当前唯一 Thread
    /// 这里存储系统中所有进程，无论其状态如何
    // timer softirq 会扫描任务表；IRQ-safe rwlock 防止打断 task-context 读写后同 hart 再入。
    tasks: IrqRwLock<BTreeMap<usize, Arc<TaskControlBlock>>>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            tasks: IrqRwLock::new(BTreeMap::new()),
        }
    }

    /// 添加新进程到系统
    /// 这是创建进程的统一入口点
    pub fn add_task(&self, task: Arc<TaskControlBlock>) {
        let tgid = task.tgid();

        // 添加到全局进程表
        {
            let mut tasks = self.tasks.write();
            let previous = tasks.insert(tgid, task.clone());
            assert!(
                previous.is_none(),
                "duplicate TGID inserted into process index"
            );
        }

        // 添加到多核调度器（根据当前状态）
        let status = *task.scheduling.status.lock();
        match status {
            TaskStatus::Ready => {
                add_task_to_best_cpu(task);
            }
            TaskStatus::Sleeping => {}
            TaskStatus::Stopped => {}
            TaskStatus::Running => {}
            TaskStatus::Exited => {}
        }
    }

    /// 根据 TGID 查找进程的当前唯一 Thread。
    /// 这是查找进程的统一接口，性能优化的O(log n)查找
    pub fn find_task_by_tgid(&self, tgid: usize) -> Option<Arc<TaskControlBlock>> {
        let processes = self.tasks.read();
        processes.get(&tgid).cloned()
    }

    fn remove_task(&self, tgid: usize) -> Option<Arc<TaskControlBlock>> {
        self.tasks.write().remove(&tgid)
    }

    /// 更新进程状态
    /// 当进程状态发生变化时，需要调用此函数来维护一致性
    pub fn update_task_status(&self, tgid: usize, old_status: TaskStatus, new_status: TaskStatus) {
        if let Some(task) = self.find_task_by_tgid(tgid) {
            // 根据状态变化进行相应的调度器操作
            match (old_status, new_status) {
                (TaskStatus::Ready, TaskStatus::Running) => {
                    // 从调度器队列移动到某个核心的current，由调度器处理
                }
                (TaskStatus::Ready, TaskStatus::Stopped) => {
                    // 从调度器队列移动到停止状态，不参与调度
                    // 任务已经从调度器中移除，无需额外操作
                }
                (TaskStatus::Running, TaskStatus::Ready) => {
                    add_task_to_best_cpu(task);
                }
                (TaskStatus::Running, TaskStatus::Sleeping) => {
                    // 从某个核心的current移动到睡眠队列，由 timer 模块处理
                }
                (TaskStatus::Running, TaskStatus::Stopped) => {
                    // 从某个核心的current移动到停止状态，不参与调度
                }
                (TaskStatus::Sleeping, TaskStatus::Ready) => {
                    add_task_to_best_cpu(task);
                }
                (TaskStatus::Sleeping, TaskStatus::Stopped) => {
                    // 从睡眠状态移动到停止状态，不参与调度
                }
                (TaskStatus::Stopped, TaskStatus::Ready) => {
                    add_task_to_best_cpu(task);
                }
                (_, TaskStatus::Exited) => {
                    // 进程退出，不需要调度
                }
                _ => {
                    // 其他状态转换
                }
            }
        }
    }

    /// 添加任务到睡眠状态
    pub fn add_sleeping_task(&self, task: Arc<TaskControlBlock>, wake_time_ns: u64) {
        // 直接在任务的wake_time_ns字段设置唤醒时间
        task.scheduling
            .wake_time_ns
            .store(wake_time_ns, Ordering::Release);
    }

    /// @description 在固定容量 batch 中唤醒 deadline 已到的 task，不在 interrupt context 分配。
    ///
    /// @param current_time_ns 当前 monotonic 时间。
    /// @return 本次唤醒的 task 数量。
    /// @errors 无可恢复错误；task status/runqueue 的完整原子事务由 Phase 6 收敛。
    pub fn wake_expired_tasks(&self, current_time_ns: u64) -> usize {
        const WAKE_BATCH: usize = 32;
        let mut total = 0;

        loop {
            let mut batch: [Option<Arc<TaskControlBlock>>; WAKE_BATCH] =
                [const { None }; WAKE_BATCH];
            let mut count = 0;
            {
                let tasks = self.tasks.read();
                for task in tasks.values() {
                    if count == WAKE_BATCH {
                        break;
                    }
                    if *task.scheduling.status.lock() != TaskStatus::Sleeping {
                        continue;
                    }
                    let wake_time = task.scheduling.wake_time_ns.load(Ordering::Acquire);
                    if wake_time == 0 || wake_time > current_time_ns {
                        continue;
                    }
                    // AcqRel 只让一个扫描者消费该 deadline；缺失 CAS 会把同一 task 重复入队。
                    if task
                        .scheduling
                        .wake_time_ns
                        .compare_exchange(wake_time, 0, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        batch[count] = Some(task.clone());
                        count += 1;
                    }
                }
            }
            for task in batch.into_iter().flatten() {
                set_task_status(&task, TaskStatus::Ready);
                total += 1;
            }
            if count < WAKE_BATCH {
                return total;
            }
        }
    }
}

// 全局 PID 索引；不宣称统一拥有 scheduler/current/status。
lazy_static! {
    pub static ref TASK_MANAGER: TaskManager = TaskManager::new();
}

/// 添加任务到系统
pub fn add_task(task: Arc<TaskControlBlock>) {
    TASK_MANAGER.add_task(task);
}

/// 根据 TGID 查找进程的当前唯一 Thread。
pub fn find_task_by_tgid(tgid: usize) -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.find_task_by_tgid(tgid)
}

/// @description 从唯一 TGID index 移除 terminal task owner。
///
/// @param tgid 当前单线程 Process 的 TGID。
/// @return index 中原有的 Task Arc；不存在时返回 `None`。
fn remove_task(tgid: usize) -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.remove_task(tgid)
}

/// 添加任务到睡眠队列
pub fn add_sleeping_task(task: Arc<TaskControlBlock>, wake_time_ns: u64) {
    TASK_MANAGER.add_sleeping_task(task, wake_time_ns);
}

/// 安全的状态更新函数
pub fn set_task_status(task: &Arc<TaskControlBlock>, new_status: TaskStatus) {
    let old_status = {
        let mut status_guard = task.scheduling.status.lock();
        let old = *status_guard;
        *status_guard = new_status;
        old
    };
    // 根据状态转换维护当前 CFS runqueue；完整单一状态事务留给 Phase 6。
    if old_status != new_status {
        TASK_MANAGER.update_task_status(task.tgid(), old_status, new_status);
    }
}

/// @description 唤醒所有到期 task，interrupt context 中不进行 heap allocation。
///
/// @param current_time_ns 当前 monotonic 时间。
/// @return 唤醒数量。
pub fn wake_expired_tasks(current_time_ns: u64) -> usize {
    TASK_MANAGER.wake_expired_tasks(current_time_ns)
}

/// @description 在 idle/scheduler stack 上终止尚未切入的 task。
///
/// @param task 从 runqueue 取出、当前不占用自身 kernel stack 的 Task。
/// @param _exit_code 当前无 wait4 ABI，因此退出码没有标准消费者。
/// @return 无返回值；TGID index 缺失表示 terminal ownership 已被其他路径消费。
fn terminate_unscheduled_task(task: &Arc<TaskControlBlock>, _exit_code: i32) {
    set_task_status(task, TaskStatus::Exited);
    let removed = remove_task(task.tgid()).expect("terminal task missing from TGID index");
    assert!(
        Arc::ptr_eq(&removed, task),
        "TGID index points to another task"
    );
    drop(removed);
}

// nanosleep 实现
pub fn nanosleep(nanoseconds: u64) -> isize {
    if nanoseconds == 0 {
        return 0;
    }
    let start_time = get_time_ns();
    // 无论时间长短，都使用睡眠队列来保证准确性
    if let Some(current_task) = current_task() {
        let wake_time = start_time + nanoseconds;

        set_task_status(&current_task, TaskStatus::Sleeping);
        add_sleeping_task(current_task, wake_time);
        // 让出CPU，等待被唤醒（此时任务状态为Sleeping，不会被重新加入就绪队列）
        block_current_and_run_next();

        // 醒来后检查实际时间
        let end_time = get_time_ns();
        let actual_sleep = end_time - start_time;

        // 检查是否睡眠时间足够
        if actual_sleep < nanoseconds {
            // 睡眠被提前中断，检查是否还需要继续睡眠
            let remaining_ns = nanoseconds - actual_sleep;
            if let Some(current_task) = task::current_task() {
                // 检查唤醒时间是否被清零（表示被信号中断）
                let wake_time = current_task.scheduling.wake_time_ns.load(Ordering::Acquire);
                if wake_time == 0 {
                    // 唤醒时间被清零，说明被信号中断，返回 EINTR
                    return -4; // EINTR
                } else {
                    // 继续睡眠剩余时间
                    return nanosleep(remaining_ns);
                }
            }
        }
    } else {
        // 如果没有当前任务，使用忙等待（不推荐，但作为备用方案）
        let start_time = get_time_ns();
        while get_time_ns() - start_time < nanoseconds {
            // 忙等待
        }
    }

    0
}

/// 获取并移除当前任务
pub fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    with_current_processor(|processor| processor.current.take())
}

/// 获取当前任务的引用
pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    with_current_processor(|processor| processor.current.clone())
}

/// 获取当前工作目录
pub fn current_cwd() -> alloc::string::String {
    current_task()
        .map(|task| task.cwd())
        .unwrap_or_else(|| "/".to_string())
}

pub fn run_tasks() -> ! {
    with_current_processor(|processor| processor.mark_active());

    loop {
        with_current_processor(|processor| processor.drain_inbound_to_local());
        let task = with_current_processor(Processor::fetch_task);

        if let Some(task) = task {
            if !task.is_exited() {
                if task.thread_signals().lock().has_deliverable() {
                    if !handle_task_signals(&task) {
                        continue;
                    }
                }
                if *task.scheduling.status.lock() == TaskStatus::Sleeping {
                    continue;
                }
                switch_to_task(task);
                continue;
            }
        }

        use riscv::asm::wfi;
        wfi();
    }
}

/// 处理任务信号
/// 返回是否应该继续调度这个任务
fn handle_task_signals(task: &Arc<TaskControlBlock>) -> bool {
    use crate::signal::handle_signals;

    let (should_continue, exit_code) = handle_signals(task, None);

    if !should_continue {
        if let Some(code) = exit_code {
            debug!(
                "Task {} terminated by signal with exit code {}",
                task.tgid(),
                code
            );
            // 执行任务清理，但不调度（因为我们在调度循环中）
            terminate_unscheduled_task(task, code);
        }
        return false; // 不应该继续调度
    }

    // 检查任务是否被信号停止（例如 SIGTSTP/Ctrl+Z）
    if *task.scheduling.status.lock() == TaskStatus::Stopped {
        debug!("Task {} was stopped by signal", task.tgid());
        return false; // 被停止的任务不应该被调度
    }

    true // 可以继续调度
}

/// 挂起当前任务并运行下一个任务
pub fn suspend_current_and_run_next() {
    // 安全地获取当前任务
    let task = match take_current_task() {
        Some(t) => t,
        None => {
            // 没有当前任务，可能在idle循环中，直接返回
            return;
        }
    };

    // 验证任务有效性
    if task.is_exited() {
        // Exited task 不应重新进入调度队列。
        return;
    }

    // 更新 CFS 使用的运行时间。
    let end_time = get_time_us();
    let mut sched = task.scheduling.policy.lock();
    let last_runtime = sched.last_runtime;
    if last_runtime > 0 && end_time > last_runtime {
        let runtime = end_time - last_runtime;
        sched.update_vruntime(runtime);
    }
    drop(sched);

    crate::signal::clear_task_on_core(hart_id(), task.tgid());
    {
        let mut status = task.scheduling.status.lock();
        if *status == TaskStatus::Running {
            *status = TaskStatus::Ready;
        }
    }
    with_current_processor(|processor| processor.add_task(task.clone()));

    // 安全的上下文切换：使用Arc引用保证内存安全
    schedule_with_task_context(task);
}

/// 安全的调度函数，确保在切换期间任务上下文内存保持有效
/// 通过保持Arc引用而非锁来保证内存安全
fn schedule_with_task_context(task: Arc<TaskControlBlock>) {
    // 只提取稳定 raw pointer，确保 `&mut Processor` 不跨越实际执行任意代码的 context switch。
    let idle_task_cx_ptr = with_current_processor(Processor::idle_context_ptr);

    // 获取任务上下文指针但立即释放锁
    let task_cx_ptr = {
        let mut task_cx = task.task_context().lock();
        let ptr = &mut *task_cx as *mut TaskContext;

        // 验证指针有效性
        if ptr.is_null() {
            panic!("Task context pointer is null for task {}", task.tid());
        }

        ptr
    }; // 锁在此处自动释放

    // TaskManager 以及 ready runqueue/sleep index 中的 owner 保证 raw context 在切换期间存活。
    // 此处必须先释放 task-stack Arc；否则 task 若不再恢复，该 Arc 会永久埋在自身 stack。
    drop(task);
    unsafe {
        crate::task::__switch(task_cx_ptr, idle_task_cx_ptr);
    }
}

/// 阻塞当前任务并切换到下一个任务
pub fn block_current_and_run_next() {
    let task = take_current_task().expect("No current task to block");

    crate::signal::clear_task_on_core(hart_id(), task.tgid());

    let end_time = get_time_us();
    let mut sched = task.scheduling.policy.lock();
    let runtime = end_time.saturating_sub(sched.last_runtime);
    sched.update_vruntime(runtime);
    drop(sched);

    {
        let mut status = task.scheduling.status.lock();
        *status = TaskStatus::Sleeping;
    }

    // 安全的上下文切换：使用Arc引用保证内存安全
    schedule_with_task_context(task);
}

/// 退出当前任务并运行下一个任务
pub fn exit_current_and_run_next(_exit_code: i32) -> ! {
    let task = take_current_task().expect("No current task to exit");

    crate::signal::clear_task_on_core(hart_id(), task.tgid());
    set_task_status(&task, TaskStatus::Exited);
    let removed = remove_task(task.tgid()).expect("exiting task missing from TGID index");
    assert!(
        Arc::ptr_eq(&removed, &task),
        "TGID index points to another task"
    );
    drop(removed);

    let idle_task_cx_ptr = with_current_processor(Processor::idle_context_ptr);
    let task_cx_ptr = {
        let mut task_cx = task.task_context().lock();
        &mut *task_cx as *mut TaskContext
    };

    // 1. owning Arc 必须先移交 per-hart slot，task stack 上不能留下永不恢复的 owner。
    crate::task::processor::defer_task_reap(task);
    // 2. 切回 idle stack；switch_to_task 的返回点负责 drain slot 并安全 Drop kernel stack。
    unsafe { crate::task::__switch(task_cx_ptr, idle_task_cx_ptr) };
    panic!("exited task context resumed")
}

/// 切换到指定任务
fn switch_to_task(task: Arc<TaskControlBlock>) {
    {
        let selected = with_current_processor(|processor| {
            if processor.current.is_some() {
                return false;
            }
            let mut status = task.scheduling.status.lock();
            if *status != TaskStatus::Ready {
                return false;
            }
            *status = TaskStatus::Running;
            processor.current = Some(task.clone());
            true
        });
        if !selected {
            return;
        }
    }

    let start_time = get_time_us();
    task.scheduling.policy.lock().last_runtime = start_time;
    // last_cpu 只记录下次调度 hint，不发布 task 内部状态。
    task.scheduling.last_cpu.store(hart_id(), Ordering::Relaxed);

    crate::signal::update_task_on_core(hart_id(), task.tgid());

    // 只保留 raw context 地址，避免 mutable Processor borrow 跨越切换。
    let idle_task_cx_ptr = with_current_processor(Processor::idle_context_ptr);

    // 获取任务上下文地址
    let next_task_cx_ptr = {
        let task_cx = task.task_context().lock();
        &*task_cx as *const TaskContext
    };

    // 验证指针有效性
    if next_task_cx_ptr.is_null() {
        panic!("Invalid task context pointer");
    }

    // 所有 guard 已释放，只携带由 task Arc 保活的 context raw pointer。
    unsafe {
        crate::task::__switch(idle_task_cx_ptr, next_task_cx_ptr);
    }
    // 退出 task 把自身 Arc 留在 per-hart slot；这里只在已经恢复的 idle stack 上析构。
    crate::task::processor::reap_deferred_task();
}
