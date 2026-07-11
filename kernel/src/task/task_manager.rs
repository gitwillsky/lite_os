use core::sync::atomic::Ordering;

use alloc::{collections::BTreeMap, sync::Arc};
use lazy_static::lazy_static;

use crate::{
    arch::hart::hart_id,
    sync::{IrqMutex, IrqRwLock, LocalIrqGuard},
    task::{
        Processor, RunState, TaskControlBlock, context::TaskContext, processor::enqueue_new_task,
        scheduler::cfs_scheduler::RunQueueEntry, with_current_processor,
    },
    timer::{get_time_ns, get_time_us},
};

/// 系统中存活 Process 的 TGID 索引。
///
/// 该表只证明 task 存在；`SchedulingState` 才是 current/runqueue/wait membership 权威。
pub struct TaskManager {
    /// 全局进程表：TGID -> 当前唯一 Thread
    // IRQ-safe rwlock 防止 timer 与 task-context 查询在同 hart 重入。
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

        enqueue_new_task(task);
    }

    fn remove_task(&self, tgid: usize) -> Option<Arc<TaskControlBlock>> {
        self.tasks.write().remove(&tgid)
    }
}

struct DeadlineWaitQueue {
    next_sequence: u64,
    entries: BTreeMap<(u64, u64), Arc<TaskControlBlock>>,
}

impl DeadlineWaitQueue {
    fn new() -> Self {
        Self {
            next_sequence: 0,
            entries: BTreeMap::new(),
        }
    }

    fn insert(&mut self, deadline: u64, task: Arc<TaskControlBlock>) -> (u64, u64) {
        self.next_sequence = self.next_sequence.wrapping_add(1);
        assert_ne!(self.next_sequence, 0, "deadline wait sequence wrapped");
        let key = (deadline, self.next_sequence);
        assert!(self.entries.insert(key, task).is_none());
        key
    }

    fn pop_expired(&mut self, now: u64) -> Option<((u64, u64), Arc<TaskControlBlock>)> {
        let key = *self.entries.first_key_value()?.0;
        if key.0 > now {
            return None;
        }
        self.entries.remove(&key).map(|task| (key, task))
    }
}

// TGID index 只拥有 Process 存活性；SchedulingState 是运行/membership 唯一权威。
lazy_static! {
    pub static ref TASK_MANAGER: TaskManager = TaskManager::new();
    static ref DEADLINE_WAIT_QUEUE: IrqMutex<DeadlineWaitQueue> =
        IrqMutex::new(DeadlineWaitQueue::new());
}

/// 添加任务到系统
pub fn add_task(task: Arc<TaskControlBlock>) {
    TASK_MANAGER.add_task(task);
}

/// @description 从唯一 TGID index 移除 terminal task owner。
///
/// @param tgid 当前单线程 Process 的 TGID。
/// @return index 中原有的 Task Arc；不存在时返回 `None`。
fn remove_task(tgid: usize) -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.remove_task(tgid)
}

/// @description 从显式 deadline wait queue 消费所有到期 task。
///
/// @param current_time_ns 当前 monotonic 时间。
/// @return 唤醒数量。
pub fn wake_expired_tasks(current_time_ns: u64) -> usize {
    const WAKE_BATCH: usize = 32;
    let mut count = 0;
    for _ in 0..WAKE_BATCH {
        let expired = DEADLINE_WAIT_QUEUE.lock().pop_expired(current_time_ns);
        let Some((key, task)) = expired else {
            return count;
        };
        if crate::task::processor::wake_deadline_task(task, key) {
            count += 1;
        }
    }
    count
}

/// @description 在明确 deadline wait queue 上阻塞当前 task。
///
/// @param nanoseconds 相对 monotonic 睡眠时长。
/// @return deadline 到期返回 0；提前唤醒返回 -EINTR，overflow 返回 -EINVAL。
pub fn nanosleep(nanoseconds: u64) -> isize {
    if nanoseconds == 0 {
        return 0;
    }
    let start_time = get_time_ns();
    let Some(deadline) = start_time.checked_add(nanoseconds) else {
        return -22;
    };
    block_current_until(deadline);
    if get_time_ns() >= deadline { 0 } else { -4 }
}

/// 获取并移除当前任务
pub fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    with_current_processor(|processor| processor.current.take())
}

/// 获取当前任务的引用
pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    with_current_processor(|processor| processor.current.clone())
}

pub fn run_tasks() -> ! {
    with_current_processor(|processor| processor.mark_active());

    loop {
        // 1. 关中断覆盖 drain/select/WFI，避免 IPI 恰好落在空队列检查与 WFI 之间而丢失 idle wake。
        let idle_irq = LocalIrqGuard::disable();
        with_current_processor(|processor| processor.drain_inbound_to_local());
        let task = with_current_processor(Processor::select_task);

        if let Some(task) = task {
            drop(idle_irq);
            switch_to_task(task);
            continue;
        }

        use riscv::asm::wfi;
        wfi();
        drop(idle_irq);
    }
}

/// 挂起当前任务并运行下一个任务
pub fn suspend_current_and_run_next() {
    let Some(task) = current_task() else {
        return;
    };
    let cpu = hart_id();

    // 更新 CFS 使用的运行时间。
    let end_time = get_time_us();
    let mut sched = task.scheduling.policy.lock();
    let last_runtime = sched.last_runtime;
    if last_runtime > 0 && end_time > last_runtime {
        let runtime = end_time - last_runtime;
        sched.update_vruntime(runtime);
    }
    drop(sched);
    let vruntime = task.scheduling.policy.lock().vruntime;

    with_current_processor(|processor| {
        let current = processor
            .current
            .take()
            .expect("yield requires current task");
        assert!(
            Arc::ptr_eq(&current, &task),
            "processor current changed during yield"
        );
        let entry = {
            let mut scheduling = task.scheduling.state.lock();
            match scheduling.run_state {
                RunState::Running { cpu: owner } => {
                    assert_eq!(owner, cpu, "running task owned by another CPU");
                    let generation = scheduling.transition_to_ready(cpu);
                    Some(RunQueueEntry {
                        task: current,
                        generation,
                        vruntime,
                    })
                }
                state => panic!("cannot suspend task in state {state:?}"),
            }
        };
        if let Some(entry) = entry {
            processor.add_ready_entry(entry);
        }
    });

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

/// @description 将当前 task 加入 deadline wait queue 并切回 idle。
///
/// @param deadline 绝对 monotonic 纳秒 deadline。
/// @return task 被唤醒并重新调度后返回。
fn block_current_until(deadline: u64) {
    let task = current_task().expect("deadline wait requires current task");
    let cpu = hart_id();

    let end_time = get_time_us();
    let mut sched = task.scheduling.policy.lock();
    let runtime = end_time.saturating_sub(sched.last_runtime);
    sched.update_vruntime(runtime);
    drop(sched);

    with_current_processor(|processor| {
        let current = processor
            .current
            .take()
            .expect("block requires current task");
        assert!(
            Arc::ptr_eq(&current, &task),
            "processor current changed during block"
        );
        let mut scheduling = task.scheduling.state.lock();
        assert_eq!(
            scheduling.run_state,
            RunState::Running { cpu },
            "only running task can block"
        );
        assert!(
            scheduling.deadline_wait.is_none(),
            "task already owns a wait membership"
        );
        // state lock 覆盖 queue insertion；waker 看到 wait key 时 entry 必然已经存在。
        let key = DEADLINE_WAIT_QUEUE.lock().insert(deadline, current);
        scheduling.deadline_wait = Some(key);
        scheduling.run_state = RunState::Blocking { cpu };
    });

    schedule_with_task_context(task);
}

/// 退出当前任务并运行下一个任务
pub fn exit_current_and_run_next(_exit_code: i32) -> ! {
    let task = take_current_task().expect("No current task to exit");

    {
        let mut scheduling = task.scheduling.state.lock();
        assert!(
            matches!(scheduling.run_state, RunState::Running { .. }),
            "only current running task can exit"
        );
        assert!(
            scheduling.deadline_wait.is_none(),
            "running task cannot retain wait membership"
        );
        scheduling.run_state = RunState::Exited;
    };
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
    let cpu = hart_id();
    with_current_processor(|processor| {
        let current = processor
            .current
            .as_ref()
            .expect("selected task missing from current");
        assert!(
            Arc::ptr_eq(current, &task),
            "selected task differs from current"
        );
    });
    assert_eq!(
        task.scheduling.state.lock().run_state,
        RunState::Running { cpu },
        "selected task must be Running on this CPU"
    );

    let start_time = get_time_us();
    task.scheduling.policy.lock().last_runtime = start_time;
    // last_cpu 只记录下次调度 hint，不发布 task 内部状态。
    task.scheduling.last_cpu.store(hart_id(), Ordering::Relaxed);

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
    crate::task::processor::finish_blocking_transition(&task);
    // 退出 task 把自身 Arc 留在 per-hart slot；这里只在已经恢复的 idle stack 上析构。
    crate::task::processor::reap_deferred_task();
}
