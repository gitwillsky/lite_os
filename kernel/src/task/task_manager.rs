use core::sync::atomic::Ordering;

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use lazy_static::lazy_static;

use crate::{
    arch::hart::{self, hart_id},
    ipc::{Pipe, PipeDirection, PipeEnd, PipeNotifier},
    sync::{IrqMutex, LocalIrqGuard},
    task::{
        PendingSignal, Processor, RunState, TaskControlBlock, WaitMembership, WaitResult,
        context::TaskContext,
        pid::{INIT_PID, ProcessId},
        processor::{begin_preempt_running_task, enqueue_new_task, request_reschedule},
        with_current_processor,
    },
    timer::{get_time_ns, get_time_us},
};

mod process_exit;
mod process_group;
mod procfs;
mod signal;
mod terminal_access;
mod wait_child;

use process_exit::ProcessExitStatus;
pub(crate) use process_exit::{
    exit_current_group, exit_current_group_by_signal, exit_current_if_group_exiting,
    exit_current_thread,
};
pub(crate) use process_group::{
    ProcessGroupError, SetProcessGroupError, claim_controlling_terminal, create_session,
    process_group, session_id, set_process_group, set_terminal_foreground_group,
    terminal_foreground_group,
};
pub(in crate::task) use process_group::{current_process_group_is_orphaned, mark_process_exec};
pub(crate) use procfs::KernelProcSource;
use signal::{ChildEvents, JobControlState};
pub(crate) use signal::{
    SignalSendError, send_process_signal, send_thread_signal, send_tid_signal, stop_current_process,
};
use signal::{complete_process_stop, send_kernel_process_signal, send_process_group_signal};
pub(crate) use terminal_access::{TerminalAccessError, check_terminal_access};
pub(crate) use wait_child::{WaitChildError, consume_child_status, wait_child};

enum ProcessState {
    Live(BTreeMap<usize, Arc<TaskControlBlock>>),
    Exited(ProcessExitStatus),
}
struct ProcessNode {
    parent: Option<usize>,
    session: usize,
    process_group: usize,
    // 标记 exec point-of-no-return；缺少它会让 parent 在新映像生效后仍成功 setpgid。
    has_execed: bool,
    state: ProcessState,
    group_exit: Option<ProcessExitStatus>,
    job_control: JobControlState,
    child_events: ChildEvents,
    waiter: Option<Arc<TaskControlBlock>>,
}
struct ProcessGraph {
    next_pid: usize,
    processes_created: u64,
    nodes: BTreeMap<usize, ProcessNode>,
}

/// @description parent relation、live task 或最小 exit record 的唯一 process graph owner。
struct TaskManager {
    graph: IrqMutex<ProcessGraph>,
    load_average: IrqMutex<LoadAverage>,
}

struct LoadAverage {
    last_update_us: u64,
    fixed: [u64; 3],
}

impl LoadAverage {
    const FIXED_ONE: u64 = 2048;
    const INTERVAL_US: u64 = 5_000_000;
    const EXP: [u64; 3] = [1884, 2014, 2037];

    fn sample(&mut self, now_us: u64, runnable: usize) -> [u64; 3] {
        while now_us.saturating_sub(self.last_update_us) >= Self::INTERVAL_US {
            let active = (runnable as u64).saturating_mul(Self::FIXED_ONE);
            for (load, exp) in self.fixed.iter_mut().zip(Self::EXP) {
                *load = load
                    .saturating_mul(exp)
                    .saturating_add(active.saturating_mul(Self::FIXED_ONE - exp))
                    / Self::FIXED_ONE;
            }
            self.last_update_us = self.last_update_us.saturating_add(Self::INTERVAL_US);
        }
        self.fixed
            .map(|load| load.saturating_mul(1000) / Self::FIXED_ONE)
    }

    fn values(&self) -> [u64; 3] {
        self.fixed
            .map(|load| load.saturating_mul(1000) / Self::FIXED_ONE)
    }
}

impl TaskManager {
    fn new() -> Self {
        Self {
            graph: IrqMutex::new(ProcessGraph {
                next_pid: INIT_PID + 1,
                processes_created: 1,
                nodes: BTreeMap::new(),
            }),
            load_average: IrqMutex::new(LoadAverage {
                last_update_us: 0,
                fixed: [0; 3],
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
                session: INIT_PID,
                process_group: INIT_PID,
                has_execed: true,
                state: ProcessState::Live(threads),
                group_exit: None,
                job_control: JobControlState::Running,
                child_events: ChildEvents::default(),
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
        let parent_node = graph
            .nodes
            .get(&parent)
            .expect("fork parent disappeared before child publication");
        assert!(matches!(parent_node.state, ProcessState::Live(_)));
        let session = parent_node.session;
        let process_group = parent_node.process_group;
        let mut threads = BTreeMap::new();
        threads.insert(child.tid(), child);
        let previous = graph.nodes.insert(
            pid,
            ProcessNode {
                parent: Some(parent),
                session,
                process_group,
                has_execed: false,
                state: ProcessState::Live(threads),
                group_exit: None,
                job_control: JobControlState::Running,
                child_events: ChildEvents::default(),
                waiter: None,
            },
        );
        assert!(previous.is_none(), "allocated PID already exists");
        graph.processes_created = graph.processes_created.saturating_add(1);
    }

    fn publish_thread(&self, tgid: usize, thread: Arc<TaskControlBlock>) -> bool {
        let mut graph = self.graph.lock();
        let node = graph
            .nodes
            .get_mut(&tgid)
            .expect("thread group missing from process graph");
        let stopping = node.group_exit.is_none() && node.job_control != JobControlState::Running;
        let ProcessState::Live(threads) = &mut node.state else {
            panic!("cannot publish thread into exited process");
        };
        assert!(threads.insert(thread.tid(), thread.clone()).is_none());
        if stopping {
            crate::task::processor::request_task_stop(&thread);
        }
        stopping
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

#[derive(Clone, Copy)]
enum IndexedWaitKind {
    Deadline,
    Futex {
        tgid: usize,
        address: usize,
    },
    Console,
    Signal {
        mask: u64,
    },
    Pipe {
        identity: usize,
        direction: PipeDirection,
    },
    Poll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PollWaitKey {
    Console,
    Pipe {
        identity: usize,
        direction: PipeDirection,
    },
}

impl PollWaitKey {
    pub(crate) fn pipe(pipe: &Arc<Pipe>, direction: PipeDirection) -> Self {
        Self::Pipe {
            identity: Pipe::identity(pipe),
            direction,
        }
    }
}

struct IndexedWaitEntry {
    task: Arc<TaskControlBlock>,
    kind: IndexedWaitKind,
    deadline: Option<u64>,
    poll_keys: Option<alloc::vec::Vec<PollWaitKey>>,
}

struct IndexedWaitQueue {
    next_id: u64,
    entries: BTreeMap<u64, IndexedWaitEntry>,
    futex_index: BTreeSet<(usize, usize, u64)>,
    deadline_index: BTreeSet<(u64, u64)>,
    console_index: BTreeSet<u64>,
    pipe_index: BTreeSet<(usize, u8, u64)>,
}

impl IndexedWaitQueue {
    fn new() -> Self {
        Self {
            next_id: 0,
            entries: BTreeMap::new(),
            futex_index: BTreeSet::new(),
            deadline_index: BTreeSet::new(),
            console_index: BTreeSet::new(),
            pipe_index: BTreeSet::new(),
        }
    }

    fn allocate_id(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        assert_ne!(self.next_id, 0, "indexed wait ID wrapped");
        self.next_id
    }

    fn insert_deadline(&mut self, deadline: u64, task: Arc<TaskControlBlock>) -> u64 {
        let id = self.allocate_id();
        assert!(self.deadline_index.insert((deadline, id)));
        assert!(
            self.entries
                .insert(
                    id,
                    IndexedWaitEntry {
                        task,
                        kind: IndexedWaitKind::Deadline,
                        deadline: Some(deadline),
                        poll_keys: None,
                    },
                )
                .is_none()
        );
        id
    }

    fn insert_futex(
        &mut self,
        tgid: usize,
        address: usize,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> u64 {
        let id = self.allocate_id();
        assert!(self.futex_index.insert((tgid, address, id)));
        if let Some(deadline) = deadline {
            assert!(self.deadline_index.insert((deadline, id)));
        }
        assert!(
            self.entries
                .insert(
                    id,
                    IndexedWaitEntry {
                        task,
                        kind: IndexedWaitKind::Futex { tgid, address },
                        deadline,
                        poll_keys: None,
                    },
                )
                .is_none()
        );
        id
    }

    fn insert_console(&mut self, task: Arc<TaskControlBlock>) -> u64 {
        let id = self.allocate_id();
        assert!(self.console_index.insert(id));
        assert!(
            self.entries
                .insert(
                    id,
                    IndexedWaitEntry {
                        task,
                        kind: IndexedWaitKind::Console,
                        deadline: None,
                        poll_keys: None,
                    },
                )
                .is_none()
        );
        id
    }

    fn insert_signal(
        &mut self,
        mask: u64,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> u64 {
        let id = self.allocate_id();
        if let Some(deadline) = deadline {
            assert!(self.deadline_index.insert((deadline, id)));
        }
        assert!(
            self.entries
                .insert(
                    id,
                    IndexedWaitEntry {
                        task,
                        kind: IndexedWaitKind::Signal { mask },
                        deadline,
                        poll_keys: None,
                    },
                )
                .is_none()
        );
        id
    }

    fn insert_pipe(
        &mut self,
        pipe: &Arc<Pipe>,
        direction: PipeDirection,
        task: Arc<TaskControlBlock>,
    ) -> u64 {
        let id = self.allocate_id();
        let identity = Pipe::identity(pipe);
        assert!(self.pipe_index.insert((identity, direction as u8, id)));
        assert!(
            self.entries
                .insert(
                    id,
                    IndexedWaitEntry {
                        task,
                        kind: IndexedWaitKind::Pipe {
                            identity,
                            direction,
                        },
                        deadline: None,
                        poll_keys: None,
                    },
                )
                .is_none()
        );
        id
    }

    fn insert_poll(
        &mut self,
        keys: alloc::vec::Vec<PollWaitKey>,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> u64 {
        let id = self.allocate_id();
        for key in &keys {
            match *key {
                PollWaitKey::Console => assert!(self.console_index.insert(id)),
                PollWaitKey::Pipe {
                    identity,
                    direction,
                } => assert!(self.pipe_index.insert((identity, direction as u8, id))),
            }
        }
        if let Some(deadline) = deadline {
            assert!(self.deadline_index.insert((deadline, id)));
        }
        assert!(
            self.entries
                .insert(
                    id,
                    IndexedWaitEntry {
                        task,
                        kind: IndexedWaitKind::Poll,
                        deadline,
                        poll_keys: Some(keys),
                    },
                )
                .is_none()
        );
        id
    }

    fn remove(&mut self, id: u64) -> Option<IndexedWaitEntry> {
        let entry = self.entries.remove(&id)?;
        if let IndexedWaitKind::Futex { tgid, address } = entry.kind {
            assert!(self.futex_index.remove(&(tgid, address, id)));
        }
        if matches!(entry.kind, IndexedWaitKind::Console) {
            assert!(self.console_index.remove(&id));
        }
        if let IndexedWaitKind::Pipe {
            identity,
            direction,
        } = entry.kind
        {
            assert!(self.pipe_index.remove(&(identity, direction as u8, id)));
        }
        if let Some(keys) = &entry.poll_keys {
            for key in keys {
                match *key {
                    PollWaitKey::Console => assert!(self.console_index.remove(&id)),
                    PollWaitKey::Pipe {
                        identity,
                        direction,
                    } => assert!(self.pipe_index.remove(&(identity, direction as u8, id))),
                }
            }
        }
        if let Some(deadline) = entry.deadline {
            assert!(self.deadline_index.remove(&(deadline, id)));
        }
        Some(entry)
    }

    fn take_futex(&mut self, tgid: usize, address: usize) -> Option<(u64, Arc<TaskControlBlock>)> {
        let (_, _, id) = *self
            .futex_index
            .range((tgid, address, 0)..=(tgid, address, u64::MAX))
            .next()?;
        self.remove(id).map(|entry| (id, entry.task))
    }

    fn pop_expired(&mut self, now: u64) -> Option<(u64, Arc<TaskControlBlock>, IndexedWaitKind)> {
        let (deadline, id) = *self.deadline_index.first()?;
        if deadline > now {
            return None;
        }
        self.remove(id).map(|entry| (id, entry.task, entry.kind))
    }

    fn take_console(&mut self) -> Option<(u64, IndexedWaitEntry)> {
        let id = *self.console_index.first()?;
        self.remove(id).map(|entry| (id, entry))
    }

    fn take_pipe(
        &mut self,
        identity: usize,
        direction: PipeDirection,
    ) -> Option<(u64, IndexedWaitEntry)> {
        let (_, _, id) = *self
            .pipe_index
            .range((identity, direction as u8, 0)..=(identity, direction as u8, u64::MAX))
            .next()?;
        self.remove(id).map(|entry| (id, entry))
    }
}

lazy_static! {
    // OWNER: task manager owns PID allocation, parent relation, live task/exit record and child waiter.
    static ref TASK_MANAGER: TaskManager = TaskManager::new();
    // OWNER: task manager owns one wait registration plus optional futex/deadline indexes.
    static ref INDEXED_WAIT_QUEUE: IrqMutex<IndexedWaitQueue> =
        IrqMutex::new(IndexedWaitQueue::new());
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

fn process_terminal_input() {
    let terminal = {
        let graph = TASK_MANAGER.graph.lock();
        graph.nodes.values().find_map(|node| {
            let ProcessState::Live(threads) = &node.state else {
                return None;
            };
            threads.values().next().map(|task| task.terminal())
        })
    };
    let Some(terminal) = terminal else {
        return;
    };
    if drain_terminal_input(&terminal).is_err() {
        debug!("TTY line discipline failed to drain UART input");
    }
}

/// @description 将指定 Terminal 的 raw input 送入 line discipline 并投递 foreground signals。
///
/// @param terminal console OFD 与 Process 共享的唯一 TTY owner。
/// @return drain 成功返回 `Ok(())`；设备或固定 queue 失败返回 `Err(())`。
pub(crate) fn drain_terminal_input(terminal: &crate::fs::Terminal) -> Result<(), ()> {
    let signals = terminal.drain_input().map_err(|_| ())?;
    let Some(pgid) = terminal.signal_target_group() else {
        return Ok(());
    };
    for signal in 1..=64 {
        if signals & (1u64 << (signal - 1)) != 0 {
            send_process_group_signal(pgid, signal);
        }
    }
    Ok(())
}

/// @description COW fork 当前单线程 process 并发布 child 到唯一 graph/runqueue。
///
/// @return parent 成功获得 child PID；COW/page-table 事务 OOM 时 graph 不发布 child。
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
    if !TASK_MANAGER.publish_thread(parent.tgid(), child.clone()) {
        enqueue_new_task(child);
    }
    Ok(tid)
}

/// @description futex WAIT 在 task layer 的精确结果分类。
#[derive(Debug, Clone, Copy)]
pub(crate) enum FutexWaitError {
    Again,
    Fault,
    Invalid,
    TimedOut,
    Interrupted,
}

/// @description 按 `(tgid,uaddr)` 等待用户 u32 改变，队列锁覆盖比较与 membership 发布。
///
/// @param address 4-byte aligned 用户地址。
/// @param expected 入队前必须匹配的当前值。
/// @param deadline 可选的绝对 monotonic 纳秒 deadline。
/// @return 被 wake 后返回成功；值不等、fault、对齐错误、超时或 signal interruption 返回明确分类。
pub(crate) fn futex_wait(
    address: usize,
    expected: u32,
    deadline: Option<u64>,
) -> Result<(), FutexWaitError> {
    if address == 0 || address & 3 != 0 {
        return Err(FutexWaitError::Invalid);
    }
    let task = current_task().expect("futex wait requires current task");
    let cpu = hart_id();
    let mut queue = INDEXED_WAIT_QUEUE.lock();
    let mut bytes = [0u8; 4];
    task.copy_from_user(address, &mut bytes)
        .map_err(|_| FutexWaitError::Fault)?;
    if u32::from_ne_bytes(bytes) != expected {
        return Err(FutexWaitError::Again);
    }
    if deadline.is_some_and(|value| value <= get_time_ns()) {
        return Err(FutexWaitError::TimedOut);
    }
    if task.has_deliverable_signal() {
        return Err(FutexWaitError::Interrupted);
    }

    let end_time = get_time_us();
    let mut sched = task.scheduling.policy.lock();
    let runtime = end_time.saturating_sub(sched.last_runtime);
    sched.update_vruntime(runtime);
    drop(sched);
    with_current_processor(|processor| {
        let current = processor
            .take_current()
            .expect("futex wait requires current task");
        assert!(Arc::ptr_eq(&current, &task));
        let mut scheduling = task.scheduling.state.lock();
        assert_eq!(scheduling.run_state, RunState::Running { cpu });
        assert!(scheduling.wait.is_none());
        assert!(scheduling.wait_result.is_none());
        let wait_id = queue.insert_futex(task.tgid(), address, deadline, current);
        scheduling.wait = Some(WaitMembership::Futex(wait_id));
        scheduling.run_state = RunState::Blocking { cpu };
    });
    drop(queue);
    schedule_with_task_context(task.clone());
    let result = task
        .scheduling
        .state
        .lock()
        .wait_result
        .take()
        .expect("futex waiter resumed without a wake result");
    match result {
        WaitResult::Woken => Ok(()),
        WaitResult::TimedOut => Err(FutexWaitError::TimedOut),
        WaitResult::Interrupted => Err(FutexWaitError::Interrupted),
    }
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
    let mut queue = INDEXED_WAIT_QUEUE.lock();
    for _ in 0..count {
        let Some(waiter) = queue.take_futex(tgid, address) else {
            break;
        };
        waiters.push(waiter);
    }
    drop(queue);
    let count = waiters.len();
    for (wait_id, task) in waiters {
        crate::task::processor::wake_futex_task(task, wait_id, WaitResult::Woken);
    }
    count
}

/// @description 从统一 wait registry 消费所有到期 task。
///
/// @param current_time_ns 当前 monotonic 时间。
/// @return 唤醒数量。
pub(crate) fn wake_expired_tasks(current_time_ns: u64) -> usize {
    const WAKE_BATCH: usize = 32;
    let mut count = 0;
    for _ in 0..WAKE_BATCH {
        let mut queue = INDEXED_WAIT_QUEUE.lock();
        let expired = queue.pop_expired(current_time_ns);
        drop(queue);
        let Some((wait_id, task, kind)) = expired else {
            return count;
        };
        let woke = match kind {
            IndexedWaitKind::Deadline => {
                crate::task::processor::wake_deadline_task(task, wait_id, WaitResult::TimedOut)
            }
            IndexedWaitKind::Futex { .. } => {
                crate::task::processor::wake_futex_task(task, wait_id, WaitResult::TimedOut)
            }
            IndexedWaitKind::Console => panic!("console wait cannot carry a deadline"),
            IndexedWaitKind::Signal { .. } => {
                crate::task::processor::wake_signal_task(task, WaitResult::TimedOut)
            }
            IndexedWaitKind::Pipe { .. } => panic!("pipe wait cannot carry a deadline"),
            IndexedWaitKind::Poll => {
                crate::task::processor::wake_poll_task(task, wait_id, WaitResult::TimedOut)
            }
        };
        if woke {
            count += 1;
        }
    }
    count
}

/// @description 在 user-return 或 scheduler idle context 消费全部 deferred work。
///
/// @return 无返回值；无 pending work 时为空操作。
pub(crate) fn dispatch_pending_deferred_work() {
    let work = hart::take_softirqs();
    if work == 0 {
        return;
    }
    if work & hart::TIMER_SOFTIRQ != 0 {
        wake_expired_tasks(get_time_ns());
        procfs::update_load_average(get_time_us());
    }
    if work & hart::CONSOLE_SOFTIRQ != 0 {
        process_terminal_input();
        wake_console_waiters();
    }
    request_reschedule();
}

/// @description 在统一 wait registry 中阻塞当前 console reader，封闭 read/enqueue IRQ race。
///
/// @param input_ready 在 registry owner lock 内复查 UART ring 的短闭包。
/// @return 输入已到达/IRQ 唤醒返回 `Woken`；signal cancellation 返回 `Interrupted`。
pub(crate) fn wait_for_console(input_ready: impl FnOnce() -> bool) -> WaitResult {
    let task = current_task().expect("console wait requires current task");
    let cpu = hart_id();
    let mut queue = INDEXED_WAIT_QUEUE.lock();
    if input_ready() {
        return WaitResult::Woken;
    }
    if task.has_deliverable_signal() {
        return WaitResult::Interrupted;
    }
    let end_time = get_time_us();
    let mut sched = task.scheduling.policy.lock();
    let runtime = end_time.saturating_sub(sched.last_runtime);
    sched.update_vruntime(runtime);
    drop(sched);
    with_current_processor(|processor| {
        let current = processor
            .take_current()
            .expect("console wait requires current task");
        assert!(Arc::ptr_eq(&current, &task));
        let mut scheduling = task.scheduling.state.lock();
        assert_eq!(scheduling.run_state, RunState::Running { cpu });
        assert!(scheduling.wait.is_none());
        assert!(scheduling.wait_result.is_none());
        let wait_id = queue.insert_console(current);
        scheduling.wait = Some(WaitMembership::Console(wait_id));
        scheduling.run_state = RunState::Blocking { cpu };
    });
    drop(queue);
    schedule_with_task_context(task.clone());
    task.scheduling
        .state
        .lock()
        .wait_result
        .take()
        .expect("console waiter resumed without a wake result")
}

fn wake_console_waiters() -> usize {
    let mut count = 0;
    loop {
        let waiter = INDEXED_WAIT_QUEUE.lock().take_console();
        let Some((wait_id, entry)) = waiter else {
            return count;
        };
        let woke = match entry.kind {
            IndexedWaitKind::Console => {
                crate::task::processor::wake_console_task(entry.task, wait_id)
            }
            IndexedWaitKind::Poll => {
                crate::task::processor::wake_poll_task(entry.task, wait_id, WaitResult::Woken)
            }
            _ => panic!("console index contains non-console wait"),
        };
        if woke {
            count += 1;
        }
    }
}

struct TaskPipeNotifier;

impl PipeNotifier for TaskPipeNotifier {
    fn notify(&self, pipe: &Arc<Pipe>) {
        wake_pipe_waiters(pipe);
    }
}

/// @description 创建绑定统一 task wait registry 的 anonymous pipe endpoints。
///
/// @return read/write endpoints；kernel heap 不足返回错误。
pub(crate) fn create_pipe_endpoints() -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), ()> {
    Pipe::pair(Arc::new(TaskPipeNotifier))
}

fn wake_pipe_waiters(pipe: &Arc<Pipe>) -> usize {
    let identity = Pipe::identity(pipe);
    let mut waiters = alloc::vec::Vec::new();
    let mut queue = INDEXED_WAIT_QUEUE.lock();
    for direction in [PipeDirection::Read, PipeDirection::Write] {
        let ready = match direction {
            PipeDirection::Read => pipe.readable(),
            PipeDirection::Write => pipe.writable(),
        };
        if !ready {
            continue;
        }
        while let Some(waiter) = queue.take_pipe(identity, direction) {
            waiters.push(waiter);
        }
    }
    drop(queue);
    let count = waiters.len();
    for (wait_id, entry) in waiters {
        match entry.kind {
            IndexedWaitKind::Pipe { .. } => {
                crate::task::processor::wake_pipe_task(entry.task, wait_id, WaitResult::Woken);
            }
            IndexedWaitKind::Poll => {
                crate::task::processor::wake_poll_task(entry.task, wait_id, WaitResult::Woken);
            }
            _ => panic!("pipe index contains non-pipe wait"),
        }
    }
    count
}

/// @description 在统一 wait registry 阻塞到 pipe endpoint ready 或 signal interruption。
///
/// @param pipe anonymous pipe owner。
/// @param direction read 等待 data/EOF；write 等待 space/broken reader。
/// @return ready 返回 Woken；signal 返回 Interrupted。
pub(crate) fn wait_for_pipe(pipe: &Arc<Pipe>, direction: PipeDirection) -> WaitResult {
    let task = current_task().expect("pipe wait requires current task");
    let cpu = hart_id();
    let mut queue = INDEXED_WAIT_QUEUE.lock();
    let ready = match direction {
        PipeDirection::Read => pipe.readable(),
        PipeDirection::Write => pipe.writable(),
    };
    if ready {
        return WaitResult::Woken;
    }
    if task.has_deliverable_signal() {
        return WaitResult::Interrupted;
    }
    let end_time = get_time_us();
    let mut sched = task.scheduling.policy.lock();
    let runtime = end_time.saturating_sub(sched.last_runtime);
    sched.update_vruntime(runtime);
    drop(sched);
    with_current_processor(|processor| {
        let current = processor
            .take_current()
            .expect("pipe wait requires current task");
        assert!(Arc::ptr_eq(&current, &task));
        let mut scheduling = task.scheduling.state.lock();
        assert_eq!(scheduling.run_state, RunState::Running { cpu });
        assert!(scheduling.wait.is_none());
        assert!(scheduling.wait_result.is_none());
        let wait_id = queue.insert_pipe(pipe, direction, current);
        scheduling.wait = Some(WaitMembership::Pipe(wait_id));
        scheduling.run_state = RunState::Blocking { cpu };
    });
    drop(queue);
    schedule_with_task_context(task.clone());
    task.scheduling
        .state
        .lock()
        .wait_result
        .take()
        .expect("pipe waiter resumed without a wake result")
}

/// @description 为一次 ppoll 在多个 I/O source index 上发布唯一 wait registration。
///
/// @param keys 去重前的 Pipe/Console readiness keys。
/// @param deadline 可选 absolute monotonic timeout。
/// @param ready 在 registry owner lock 内执行的无阻塞 readiness 复查。
/// @return source ready、timeout 或 signal interruption。
pub(crate) fn wait_for_poll(
    mut keys: alloc::vec::Vec<PollWaitKey>,
    deadline: Option<u64>,
    ready: impl FnOnce() -> bool,
) -> WaitResult {
    keys.sort_unstable();
    keys.dedup();
    let task = current_task().expect("ppoll wait requires current task");
    let cpu = hart_id();
    let mut queue = INDEXED_WAIT_QUEUE.lock();
    if ready() {
        return WaitResult::Woken;
    }
    if deadline.is_some_and(|value| value <= get_time_ns()) {
        return WaitResult::TimedOut;
    }
    if task.has_deliverable_signal() {
        return WaitResult::Interrupted;
    }
    let end_time = get_time_us();
    let mut sched = task.scheduling.policy.lock();
    let runtime = end_time.saturating_sub(sched.last_runtime);
    sched.update_vruntime(runtime);
    drop(sched);
    with_current_processor(|processor| {
        let current = processor
            .take_current()
            .expect("ppoll wait requires current task");
        assert!(Arc::ptr_eq(&current, &task));
        let mut scheduling = task.scheduling.state.lock();
        assert_eq!(scheduling.run_state, RunState::Running { cpu });
        assert!(scheduling.wait.is_none());
        assert!(scheduling.wait_result.is_none());
        let wait_id = queue.insert_poll(keys, deadline, current);
        scheduling.wait = Some(WaitMembership::Poll(wait_id));
        scheduling.run_state = RunState::Blocking { cpu };
    });
    drop(queue);
    schedule_with_task_context(task.clone());
    task.scheduling
        .state
        .lock()
        .wait_result
        .take()
        .expect("ppoll waiter resumed without result")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalWaitError {
    Again,
    Interrupted,
}

/// @description 在统一 wait registry 中等待并消费一个指定 pending signal。
///
/// @param mask 用户提供且已去除 SIGKILL/SIGSTOP 的 signal set。
/// @param deadline 可选 absolute monotonic deadline；None 表示无限等待。
/// @return 匹配 signal number 与 siginfo 来源。
/// @errors zero/到期 timeout 返回 Again；无关的可交付 signal 返回 Interrupted。
pub(crate) fn wait_for_signal(
    mask: u64,
    deadline: Option<u64>,
) -> Result<(usize, PendingSignal), SignalWaitError> {
    let task = current_task().expect("signal wait requires current task");
    let cpu = hart_id();
    loop {
        let mut queue = INDEXED_WAIT_QUEUE.lock();
        if let Some(signal) = task.take_pending_signal(mask) {
            return Ok(signal);
        }
        if deadline.is_some_and(|value| value <= get_time_ns()) {
            return Err(SignalWaitError::Again);
        }
        if task.has_deliverable_signal() {
            return Err(SignalWaitError::Interrupted);
        }

        let end_time = get_time_us();
        let mut sched = task.scheduling.policy.lock();
        let runtime = end_time.saturating_sub(sched.last_runtime);
        sched.update_vruntime(runtime);
        drop(sched);
        with_current_processor(|processor| {
            let current = processor
                .take_current()
                .expect("signal wait requires current task");
            assert!(Arc::ptr_eq(&current, &task));
            let mut scheduling = task.scheduling.state.lock();
            assert_eq!(scheduling.run_state, RunState::Running { cpu });
            assert!(scheduling.wait.is_none());
            assert!(scheduling.wait_result.is_none());
            let wait_id = queue.insert_signal(mask, deadline, current);
            scheduling.wait = Some(WaitMembership::Signal(wait_id));
            scheduling.run_state = RunState::Blocking { cpu };
        });
        drop(queue);
        schedule_with_task_context(task.clone());
        let result = task
            .scheduling
            .state
            .lock()
            .wait_result
            .take()
            .expect("signal waiter resumed without a wake result");
        match result {
            WaitResult::Woken => {}
            WaitResult::TimedOut => return Err(SignalWaitError::Again),
            WaitResult::Interrupted => return Err(SignalWaitError::Interrupted),
        }
    }
}

/// @description 用 Signal membership 等待 trap-return 可交付 signal，但不消费 pending bit。
///
/// @param deliverable_set sigsuspend 临时 mask 的补集。
/// @return signal 发布后返回；pending signal 留给唯一 trap delivery path。
pub(crate) fn wait_for_signal_delivery(deliverable_set: u64) {
    let task = current_task().expect("signal delivery wait requires current task");
    let cpu = hart_id();
    let mut queue = INDEXED_WAIT_QUEUE.lock();
    if task.with_pending_signal(deliverable_set, || ()).is_some() {
        return;
    }
    let end_time = get_time_us();
    let mut sched = task.scheduling.policy.lock();
    let runtime = end_time.saturating_sub(sched.last_runtime);
    sched.update_vruntime(runtime);
    drop(sched);
    with_current_processor(|processor| {
        let current = processor
            .take_current()
            .expect("signal delivery wait requires current task");
        assert!(Arc::ptr_eq(&current, &task));
        let mut scheduling = task.scheduling.state.lock();
        assert_eq!(scheduling.run_state, RunState::Running { cpu });
        assert!(scheduling.wait.is_none());
        assert!(scheduling.wait_result.is_none());
        let wait_id = queue.insert_signal(deliverable_set, None, current);
        scheduling.wait = Some(WaitMembership::Signal(wait_id));
        scheduling.run_state = RunState::Blocking { cpu };
    });
    drop(queue);
    schedule_with_task_context(task.clone());
    let result = task
        .scheduling
        .state
        .lock()
        .wait_result
        .take()
        .expect("signal delivery waiter resumed without result");
    assert_eq!(
        result,
        WaitResult::Woken,
        "sigsuspend has no timeout/cancellation path"
    );
}

/// @description 在统一 wait registry 上阻塞当前 task。
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
    match block_current_until(deadline) {
        WaitResult::TimedOut => 0,
        WaitResult::Interrupted => -4,
        WaitResult::Woken => panic!("deadline wait cannot complete through generic wake"),
    }
}

/// 获取并移除当前任务
pub(crate) fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    with_current_processor(Processor::take_current)
}

/// 获取当前任务的引用
pub(crate) fn current_task() -> Option<Arc<TaskControlBlock>> {
    with_current_processor(|processor| processor.current.clone())
}

pub(crate) fn run_tasks() -> ! {
    with_current_processor(|processor| processor.mark_active());

    loop {
        // 1. 关中断覆盖 deferred work、mailbox drain 和 task select，保证 idle 决策看到一致状态。
        let idle_irq = LocalIrqGuard::disable();
        dispatch_pending_deferred_work();
        with_current_processor(|processor| processor.drain_inbound_to_local());
        let task = with_current_processor(Processor::select_task);

        if let Some(task) = task {
            drop(idle_irq);
            switch_to_task(task);
            continue;
        }

        // 2. SIE=0 时执行 WFI，已在 sie 中单独使能的 timer/IPI 仍会让 hart 退出等待。
        // 3. 醒来后才短暂打开 SIE 投递 pending trap；若在 WFI 前开中断，trap 可能先于
        //    WFI 返回并造成 lost wakeup。
        // idle context 不继承 user trap 的 SIE=0：醒来后显式投递一次 pending interrupt。
        use riscv::asm::wfi;
        wfi();
        drop(idle_irq);
        // SAFETY: 当前 hart 没有 running task，所有 scheduler guard 已释放；只为投递本地 pending trap 修改 SIE。
        unsafe { riscv::register::sstatus::set_sie() }
        // SAFETY: 与上面的 idle-only enable 配对，恢复 kernel continuation 的 non-nested 契约。
        unsafe { riscv::register::sstatus::clear_sie() }
    }
}

/// 挂起当前任务并运行下一个任务
pub(crate) fn suspend_current_and_run_next() {
    let Some(task) = current_task() else {
        return;
    };
    // 更新 CFS 使用的运行时间。
    let end_time = get_time_us();
    let mut sched = task.scheduling.policy.lock();
    let last_runtime = sched.last_runtime;
    if last_runtime > 0 && end_time > last_runtime {
        let runtime = end_time - last_runtime;
        sched.update_vruntime(runtime);
    }
    drop(sched);
    begin_preempt_running_task(&task);

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

    // TaskManager 以及 ready runqueue/indexed wait registry 中的 owner 保证 raw context 在切换期间存活。
    // 此处必须先释放 task-stack Arc；否则 task 若不再恢复，该 Arc 会永久埋在自身 stack。
    drop(task);
    // SAFETY: both contexts are retained by per-hart/task ownership, all guards are released,
    // and the idle context is the valid continuation for this hart.
    unsafe {
        crate::task::__switch(task_cx_ptr, idle_task_cx_ptr);
    }
}

/// @description 将当前 task 加入 indexed wait registry 并切回 idle。
///
/// @param deadline 绝对 monotonic 纳秒 deadline。
/// @return task 被重新调度后返回 timeout 或 signal interruption 结果。
fn block_current_until(deadline: u64) -> WaitResult {
    let task = current_task().expect("deadline wait requires current task");
    let cpu = hart_id();

    let end_time = get_time_us();
    let mut sched = task.scheduling.policy.lock();
    let runtime = end_time.saturating_sub(sched.last_runtime);
    sched.update_vruntime(runtime);
    drop(sched);

    let mut queue = INDEXED_WAIT_QUEUE.lock();
    if task.has_deliverable_signal() {
        return WaitResult::Interrupted;
    }
    with_current_processor(|processor| {
        let current = processor
            .take_current()
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
        assert!(scheduling.wait_result.is_none());
        // state lock 覆盖 queue insertion；waker 看到 wait key 时 entry 必然已经存在。
        let wait_id = queue.insert_deadline(deadline, current);
        scheduling.wait = Some(WaitMembership::Deadline(wait_id));
        scheduling.run_state = RunState::Blocking { cpu };
    });
    drop(queue);
    schedule_with_task_context(task.clone());
    task.scheduling
        .state
        .lock()
        .wait_result
        .take()
        .expect("deadline waiter resumed without a wake result")
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
    if crate::task::processor::finish_deschedule_transition(&task) {
        complete_process_stop(task.tgid());
    }
    // 退出 task 把自身 Arc 留在 per-hart slot；这里只在已经恢复的 idle stack 上析构。
    crate::task::processor::reap_deferred_task();
}
