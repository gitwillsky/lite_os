use core::sync::atomic::Ordering;

use alloc::{collections::BTreeMap, sync::Arc};
use lazy_static::lazy_static;

use crate::{
    arch::hart::hart_id,
    sync::{IrqMutex, LocalIrqGuard},
    task::{
        Processor, RunState, TaskControlBlock, WaitMembership,
        context::TaskContext,
        pid::{INIT_PID, ProcessId},
        processor::enqueue_new_task,
        scheduler::cfs_scheduler::RunQueueEntry,
        with_current_processor,
    },
    timer::{get_time_ns, get_time_us},
};

enum ProcessState {
    Live(BTreeMap<usize, Arc<TaskControlBlock>>),
    Exited(i32),
}

struct ProcessNode {
    parent: Option<usize>,
    state: ProcessState,
    waiter: Option<Arc<TaskControlBlock>>,
}

struct ProcessGraph {
    next_pid: usize,
    nodes: BTreeMap<usize, ProcessNode>,
}

/// @description parent relation、live task 或最小 exit record 的唯一 process graph owner。
struct TaskManager {
    graph: IrqMutex<ProcessGraph>,
}

impl TaskManager {
    fn new() -> Self {
        Self {
            graph: IrqMutex::new(ProcessGraph {
                next_pid: INIT_PID + 1,
                nodes: BTreeMap::new(),
            }),
        }
    }

    fn add_init(&self, task: Arc<TaskControlBlock>) {
        let tgid = task.tgid();
        assert_eq!(tgid, INIT_PID);
        let mut threads = BTreeMap::new();
        threads.insert(task.tid(), task.clone());
        let previous = self.graph.lock().nodes.insert(
            tgid,
            ProcessNode {
                parent: None,
                state: ProcessState::Live(threads),
                waiter: None,
            },
        );
        assert!(previous.is_none(), "init inserted twice");
        enqueue_new_task(task);
    }

    fn allocate_pid(&self) -> ProcessId {
        let mut graph = self.graph.lock();
        let pid = graph.next_pid;
        graph.next_pid = graph
            .next_pid
            .checked_add(1)
            .expect("PID namespace exhausted");
        ProcessId::allocated(pid)
    }

    fn publish_child(&self, parent: usize, child: Arc<TaskControlBlock>) {
        let pid = child.tgid();
        let mut graph = self.graph.lock();
        assert!(
            matches!(
                graph.nodes.get(&parent).map(|node| &node.state),
                Some(ProcessState::Live(_))
            ),
            "fork parent disappeared before child publication"
        );
        let mut threads = BTreeMap::new();
        threads.insert(child.tid(), child);
        let previous = graph.nodes.insert(
            pid,
            ProcessNode {
                parent: Some(parent),
                state: ProcessState::Live(threads),
                waiter: None,
            },
        );
        assert!(previous.is_none(), "allocated PID already exists");
    }

    fn publish_thread(&self, tgid: usize, thread: Arc<TaskControlBlock>) {
        let mut graph = self.graph.lock();
        let node = graph
            .nodes
            .get_mut(&tgid)
            .expect("thread group missing from process graph");
        let ProcessState::Live(threads) = &mut node.state else {
            panic!("cannot publish thread into exited process");
        };
        assert!(threads.insert(thread.tid(), thread).is_none());
    }

    fn parent_pid(&self, pid: usize) -> usize {
        self.graph
            .lock()
            .nodes
            .get(&pid)
            .and_then(|node| node.parent)
            .unwrap_or(0)
    }
}

struct DeadlineWaitQueue {
    next_sequence: u64,
    entries: BTreeMap<(u64, u64), Arc<TaskControlBlock>>,
}

struct FutexWaitQueue {
    next_sequence: u64,
    entries: BTreeMap<(usize, usize, u64), Arc<TaskControlBlock>>,
}

impl FutexWaitQueue {
    fn new() -> Self {
        Self {
            next_sequence: 0,
            entries: BTreeMap::new(),
        }
    }

    fn insert(
        &mut self,
        tgid: usize,
        address: usize,
        task: Arc<TaskControlBlock>,
    ) -> (usize, usize, u64) {
        self.next_sequence = self.next_sequence.wrapping_add(1);
        assert_ne!(self.next_sequence, 0, "futex wait sequence wrapped");
        let key = (tgid, address, self.next_sequence);
        assert!(self.entries.insert(key, task).is_none());
        key
    }

    fn take(
        &mut self,
        tgid: usize,
        address: usize,
    ) -> Option<((usize, usize, u64), Arc<TaskControlBlock>)> {
        let key = *self
            .entries
            .range((tgid, address, 0)..=(tgid, address, u64::MAX))
            .next()?
            .0;
        self.entries.remove(&key).map(|task| (key, task))
    }
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

lazy_static! {
    // OWNER: task manager owns PID allocation, parent relation, live task/exit record and child waiter.
    static ref TASK_MANAGER: TaskManager = TaskManager::new();
    // OWNER: task manager owns the deadline index for sleeping tasks.
    static ref DEADLINE_WAIT_QUEUE: IrqMutex<DeadlineWaitQueue> =
        IrqMutex::new(DeadlineWaitQueue::new());
    // OWNER: task manager owns address-space-keyed futex waiter membership.
    static ref FUTEX_WAIT_QUEUE: IrqMutex<FutexWaitQueue> =
        IrqMutex::new(FutexWaitQueue::new());
}

/// @description 发布 kernel 创建的唯一 init task。
///
/// @param task TGID 必须为 INIT_PID 且尚未进入 process graph。
/// @return 无返回值。
pub(super) fn add_init_task(task: Arc<TaskControlBlock>) {
    TASK_MANAGER.add_init(task);
}

/// @description 查询 process graph 中的 parent TGID。
///
/// @param pid 当前 live process TGID。
/// @return init 或无 parent 返回零，否则返回 parent TGID。
pub(crate) fn parent_pid(pid: usize) -> usize {
    TASK_MANAGER.parent_pid(pid)
}

pub(crate) fn thread_count(tgid: usize) -> usize {
    let graph = TASK_MANAGER.graph.lock();
    match graph.nodes.get(&tgid).map(|node| &node.state) {
        Some(ProcessState::Live(threads)) => threads.len(),
        _ => 0,
    }
}

/// @description 通过唯一 process graph 定位 Thread 并合并一个 thread-directed signal。
///
/// @param tgid 目标 Thread 所属 Process ID。
/// @param tid 目标 Thread ID。
/// @param signal Linux signal number；零仅执行存在性检查。
/// @return 目标存在且 signal 合法时返回 `Ok(())`。
/// @errors Process/Thread 不存在或 signal 非法时返回 `Err(())`。
pub(crate) fn send_thread_signal(tgid: usize, tid: usize, signal: usize) -> Result<(), ()> {
    let target = {
        let graph = TASK_MANAGER.graph.lock();
        let Some(ProcessState::Live(threads)) = graph.nodes.get(&tgid).map(|node| &node.state)
        else {
            return Err(());
        };
        threads.get(&tid).cloned().ok_or(())?
    };
    if signal == 0 {
        Ok(())
    } else {
        target.queue_signal(signal)
    }
}

/// @description eager fork 当前单线程 process 并发布 child 到唯一 graph/runqueue。
///
/// @return parent 成功获得 child PID；地址空间复制 OOM 时 graph 不发布 child。
pub(crate) fn fork_current_process() -> Result<usize, crate::memory::MemoryError> {
    let parent = current_task().expect("fork requires current task");
    let pid = TASK_MANAGER.allocate_pid();
    let child = Arc::new(parent.fork_process(pid)?);
    let child_pid = child.tgid();
    TASK_MANAGER.publish_child(parent.tgid(), child.clone());
    enqueue_new_task(child);
    Ok(child_pid)
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ThreadCloneError {
    Memory(crate::memory::MemoryError),
    Fault,
}

/// @description 在当前 thread group 内创建共享 Process 资源的新 Thread。
///
/// @param stack 16-byte aligned child 用户栈顶。
/// @param tls child `tp`。
/// @param parent_tid 可选 parent TID copyout。
/// @param child_set_tid 可选 child TID copyout。
/// @param clear_child_tid 可选 thread exit 清零地址。
/// @return 成功返回 child TID；任何验证/分配失败都不发布 graph/runqueue membership。
pub(crate) fn clone_current_thread(
    stack: usize,
    tls: usize,
    parent_tid: Option<usize>,
    child_set_tid: Option<usize>,
    clear_child_tid: Option<usize>,
) -> Result<usize, ThreadCloneError> {
    let parent = current_task().expect("thread clone requires current task");
    let tid = TASK_MANAGER.allocate_pid().0;
    let child = Arc::new(
        parent
            .clone_thread(tid, stack, tls, clear_child_tid)
            .map_err(ThreadCloneError::Memory)?,
    );
    if parent
        .write_clone_tid_values([parent_tid, child_set_tid], tid as i32)
        .is_err()
    {
        child.remove_thread_trap_context();
        return Err(ThreadCloneError::Fault);
    }
    TASK_MANAGER.publish_thread(parent.tgid(), child.clone());
    enqueue_new_task(child);
    Ok(tid)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ChildExit {
    pub(crate) pid: usize,
    pub(crate) status: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitChildError {
    NoChild,
    InvalidSelector,
}

fn matching_child(child: usize, selector: isize) -> bool {
    selector == -1 || selector > 0 && child == selector as usize
}

fn find_waitable_child(
    graph: &ProcessGraph,
    parent: usize,
    selector: isize,
) -> Result<Option<ChildExit>, WaitChildError> {
    if selector == 0 || selector < -1 {
        return Err(WaitChildError::InvalidSelector);
    }
    let mut has_child = false;
    for (pid, node) in &graph.nodes {
        if node.parent != Some(parent) || !matching_child(*pid, selector) {
            continue;
        }
        has_child = true;
        if let ProcessState::Exited(code) = node.state {
            return Ok(Some(ChildExit {
                pid: *pid,
                status: (code & 0xff) << 8,
            }));
        }
    }
    if has_child {
        Ok(None)
    } else {
        Err(WaitChildError::NoChild)
    }
}

/// @description 等待指定或任一直接 child 产生最小 exit record。
///
/// @param selector `-1` 表示任一 child，正数表示指定 PID。
/// @param nohang 无可消费 record 时是否立即返回。
/// @return exit record、WNOHANG 的 None，或 selector/child 错误；record 尚未被消费。
pub(crate) fn wait_child(
    selector: isize,
    nohang: bool,
) -> Result<Option<ChildExit>, WaitChildError> {
    let task = current_task().expect("wait4 requires current task");
    let parent = task.tgid();
    loop {
        let mut graph = TASK_MANAGER.graph.lock();
        match find_waitable_child(&graph, parent, selector)? {
            Some(record) => return Ok(Some(record)),
            None if nohang => return Ok(None),
            None => {}
        }

        let cpu = hart_id();
        let end_time = get_time_us();
        let mut sched = task.scheduling.policy.lock();
        let runtime = end_time.saturating_sub(sched.last_runtime);
        sched.update_vruntime(runtime);
        drop(sched);

        // graph lock 覆盖“再次检查 child”到 waiter 发布；exit 必须取得同一锁，因此不会丢唤醒。
        with_current_processor(|processor| {
            let current = processor
                .current
                .take()
                .expect("child wait requires current task");
            assert!(Arc::ptr_eq(&current, &task));
            let mut scheduling = task.scheduling.state.lock();
            assert_eq!(scheduling.run_state, RunState::Running { cpu });
            assert!(
                scheduling.wait.is_none(),
                "task already owns wait membership"
            );
            let parent_node = graph
                .nodes
                .get_mut(&parent)
                .expect("waiting parent missing from process graph");
            assert!(
                parent_node.waiter.is_none(),
                "parent already owns child waiter"
            );
            parent_node.waiter = Some(current);
            scheduling.wait = Some(WaitMembership::Child);
            scheduling.run_state = RunState::Blocking { cpu };
        });
        drop(graph);
        schedule_with_task_context(task.clone());
    }
}

/// @description 在 status copyout 成功后消费唯一 child exit record。
///
/// @param pid `wait_child` 返回且仍属于当前 parent 的 exited child。
/// @return 成功返回空值；record 变化表示内核不变量损坏。
pub(crate) fn reap_child(pid: usize) {
    let parent = current_task().expect("reap requires current task").tgid();
    let mut graph = TASK_MANAGER.graph.lock();
    let node = graph
        .nodes
        .get(&pid)
        .expect("reaped child missing from process graph");
    assert_eq!(node.parent, Some(parent));
    assert!(matches!(node.state, ProcessState::Exited(_)));
    assert!(node.waiter.is_none());
    graph.nodes.remove(&pid);
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum FutexWaitError {
    Again,
    Fault,
    Invalid,
}

/// @description 按 `(tgid,uaddr)` 等待用户 u32 改变，队列锁覆盖比较与 membership 发布。
///
/// @param address 4-byte aligned 用户地址。
/// @param expected 入队前必须匹配的当前值。
/// @return 被 wake 后返回成功；值不等、fault 或对齐错误返回明确分类。
pub(crate) fn futex_wait(address: usize, expected: u32) -> Result<(), FutexWaitError> {
    if address == 0 || address & 3 != 0 {
        return Err(FutexWaitError::Invalid);
    }
    let task = current_task().expect("futex wait requires current task");
    let cpu = hart_id();
    let mut queue = FUTEX_WAIT_QUEUE.lock();
    let mut bytes = [0u8; 4];
    task.copy_from_user(address, &mut bytes)
        .map_err(|_| FutexWaitError::Fault)?;
    if u32::from_ne_bytes(bytes) != expected {
        return Err(FutexWaitError::Again);
    }

    let end_time = get_time_us();
    let mut sched = task.scheduling.policy.lock();
    let runtime = end_time.saturating_sub(sched.last_runtime);
    sched.update_vruntime(runtime);
    drop(sched);
    with_current_processor(|processor| {
        let current = processor
            .current
            .take()
            .expect("futex wait requires current task");
        assert!(Arc::ptr_eq(&current, &task));
        let mut scheduling = task.scheduling.state.lock();
        assert_eq!(scheduling.run_state, RunState::Running { cpu });
        assert!(scheduling.wait.is_none());
        let key = queue.insert(task.tgid(), address, current);
        scheduling.wait = Some(WaitMembership::Futex(key));
        scheduling.run_state = RunState::Blocking { cpu };
    });
    drop(queue);
    schedule_with_task_context(task);
    Ok(())
}

/// @description 唤醒同一地址空间 key 上最多 `count` 个 futex waiter。
///
/// @param tgid 地址空间 identity。
/// @param address 4-byte aligned 用户地址。
/// @param count 最大唤醒数。
/// @return 实际消费的 waiter 数。
pub(crate) fn futex_wake(tgid: usize, address: usize, count: usize) -> usize {
    if address == 0 || address & 3 != 0 || count == 0 {
        return 0;
    }
    let mut waiters = alloc::vec::Vec::new();
    let mut queue = FUTEX_WAIT_QUEUE.lock();
    for _ in 0..count {
        let Some(waiter) = queue.take(tgid, address) else {
            break;
        };
        waiters.push(waiter);
    }
    drop(queue);
    let count = waiters.len();
    for (key, task) in waiters {
        crate::task::processor::wake_futex_task(task, key);
    }
    count
}

/// @description 从显式 deadline wait queue 消费所有到期 task。
///
/// @param current_time_ns 当前 monotonic 时间。
/// @return 唤醒数量。
pub(crate) fn wake_expired_tasks(current_time_ns: u64) -> usize {
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
pub(crate) fn nanosleep(nanoseconds: u64) -> isize {
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
pub(crate) fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    with_current_processor(|processor| processor.current.take())
}

/// 获取当前任务的引用
pub(crate) fn current_task() -> Option<Arc<TaskControlBlock>> {
    with_current_processor(|processor| processor.current.clone())
}

pub(crate) fn run_tasks() -> ! {
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
pub(crate) fn suspend_current_and_run_next() {
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
    // SAFETY: both contexts are retained by per-hart/task ownership, all guards are released,
    // and the idle context is the valid continuation for this hart.
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
            scheduling.wait.is_none(),
            "task already owns a wait membership"
        );
        // state lock 覆盖 queue insertion；waker 看到 wait key 时 entry 必然已经存在。
        let key = DEADLINE_WAIT_QUEUE.lock().insert(deadline, current);
        scheduling.wait = Some(WaitMembership::Deadline(key));
        scheduling.run_state = RunState::Blocking { cpu };
    });

    schedule_with_task_context(task);
}

/// 退出当前任务并运行下一个任务
pub(crate) fn exit_current_and_run_next(exit_code: i32) -> ! {
    let task = take_current_task().expect("No current task to exit");

    {
        let mut scheduling = task.scheduling.state.lock();
        assert!(
            matches!(scheduling.run_state, RunState::Running { .. }),
            "only current running task can exit"
        );
        assert!(
            scheduling.wait.is_none(),
            "running task cannot retain wait membership"
        );
        scheduling.run_state = RunState::Exited;
    };
    task.cleanup_robust_list();
    if let Some(address) = task.take_clear_child_tid()
        && task.copy_to_user(address, &0u32.to_ne_bytes()).is_ok()
    {
        futex_wake(task.tgid(), address, 1);
    }
    let (removed, last_thread, parent_waiter, init_waiter) = {
        let mut graph = TASK_MANAGER.graph.lock();
        let exiting_pid = task.tgid();
        let (removed, last_thread, parent) = {
            let node = graph
                .nodes
                .get_mut(&exiting_pid)
                .expect("exiting task missing from process graph");
            let ProcessState::Live(threads) = &mut node.state else {
                panic!("process exited twice");
            };
            let removed = threads
                .remove(&task.tid())
                .expect("exiting thread missing from process graph");
            let last_thread = threads.is_empty();
            let parent = node.parent;
            if last_thread {
                assert!(node.waiter.is_none());
                node.state = ProcessState::Exited(exit_code);
            }
            (removed, last_thread, parent)
        };
        assert!(Arc::ptr_eq(&removed, &task));

        if !last_thread {
            (removed, false, None, None)
        } else {
            // 1. orphan 只改写 graph 中的唯一 parent edge；不复制 child collection。
            if exiting_pid != INIT_PID {
                for child in graph.nodes.values_mut() {
                    if child.parent == Some(exiting_pid) {
                        child.parent = Some(INIT_PID);
                    }
                }
            }
            // 2. 取走 waiter owner 后释放 graph lock，再进入 scheduler seam，避免锁序反转。
            let parent_waiter = parent.and_then(|pid| {
                graph
                    .nodes
                    .get_mut(&pid)
                    .and_then(|parent| parent.waiter.take())
            });
            let adopted_exited = exiting_pid != INIT_PID
                && graph.nodes.values().any(|child| {
                    child.parent == Some(INIT_PID) && matches!(child.state, ProcessState::Exited(_))
                });
            let init_waiter = adopted_exited
                .then(|| {
                    graph
                        .nodes
                        .get_mut(&INIT_PID)
                        .and_then(|init| init.waiter.take())
                })
                .flatten();
            (removed, true, parent_waiter, init_waiter)
        }
    };
    drop(removed);
    if !last_thread {
        task.remove_thread_trap_context();
    }
    for waiter in [parent_waiter, init_waiter].into_iter().flatten() {
        crate::task::processor::wake_child_task(waiter);
    }

    let idle_task_cx_ptr = with_current_processor(Processor::idle_context_ptr);
    let task_cx_ptr = {
        let mut task_cx = task.task_context().lock();
        &mut *task_cx as *mut TaskContext
    };

    // 1. owning Arc 必须先移交 per-hart slot，task stack 上不能留下永不恢复的 owner。
    crate::task::processor::defer_task_reap(task);
    // 2. 切回 idle stack；switch_to_task 的返回点负责 drain slot 并安全 Drop kernel stack。
    // SAFETY: deferred owner keeps the exiting task stack/context alive through the switch;
    // idle context is hart-local and remains valid for the kernel lifetime.
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
    // SAFETY: owning task Arc keeps the next context alive, the hart-local idle context is an
    // exclusive save target, and scheduling state prevents concurrent execution of next.
    unsafe {
        crate::task::__switch(idle_task_cx_ptr, next_task_cx_ptr);
    }
    crate::task::processor::finish_blocking_transition(&task);
    // 退出 task 把自身 Arc 留在 per-hart slot；这里只在已经恢复的 idle stack 上析构。
    crate::task::processor::reap_deferred_task();
}
