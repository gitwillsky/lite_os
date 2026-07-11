use crate::sync::{IrqMutex, LocalIrqGuard};
use crate::{
    arch::{
        hart::{MAX_CORES, hart_id},
        sbi,
    },
    task::{
        RunState, TaskControlBlock,
        context::TaskContext,
        scheduler::cfs_scheduler::{CfsRunQueue, RunQueueEntry},
    },
};
use alloc::{collections::VecDeque, sync::Arc};
use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

/// @description context switch 异常返回时的 fail-stop 目标。
///
/// @return 永不返回。
#[unsafe(no_mangle)]
pub extern "C" fn idle_return() -> ! {
    panic!("idle context returned unexpectedly");
}

/// @description 仅由所属 hart 可变访问的调度执行状态。
pub struct Processor {
    hart_id: usize,
    pub current: Option<Arc<TaskControlBlock>>,
    idle_context: TaskContext,
    runqueue: CfsRunQueue,
    deferred_reap: Option<Arc<TaskControlBlock>>,
    need_reschedule: bool,
}

impl Processor {
    fn new(hart_id: usize) -> Self {
        let mut idle_context = TaskContext::zero_init();
        idle_context.set_ra(idle_return as usize);
        Self {
            hart_id,
            current: None,
            idle_context,
            runqueue: CfsRunQueue::new(),
            deferred_reap: None,
            need_reschedule: false,
        }
    }

    /// @description 获取当前 hart idle context 的稳定地址。
    ///
    /// @return 指向本 hart `TaskContext` 的唯一可变指针。
    pub fn idle_context_ptr(&mut self) -> *mut TaskContext {
        &mut self.idle_context
    }

    /// @description 把已完成 Ready 状态转换的 entry 加入本地 runqueue。
    ///
    /// @param entry generation 必须对应 `Ready { cpu: self }`。
    /// @return 无返回值。
    pub fn add_ready_entry(&mut self, entry: RunQueueEntry) {
        self.runqueue.push(entry);
        // queued_entries 与 local heap 每次 push/pop 一一对应，不统计 mailbox/current。
        per_hart(self.hart_id)
            .queued_entries
            .fetch_add(1, Ordering::Relaxed);
        debug_assert_eq!(
            self.runqueue.len(),
            per_hart(self.hart_id)
                .queued_entries
                .load(Ordering::Relaxed)
        );
    }

    /// @description 消费 stale entry，原子完成 Ready → Running 与 current 发布。
    ///
    /// @return 队列为空时返回 `None`，否则返回唯一取出的任务引用。
    pub fn select_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        assert!(self.current.is_none(), "CPU already owns a current task");
        loop {
            let entry = self.runqueue.pop()?;
            per_hart(self.hart_id)
                .queued_entries
                .fetch_sub(1, Ordering::Relaxed);
            let mut scheduling = entry.task.scheduling.state.lock();
            match scheduling.run_state {
                RunState::Ready { cpu, generation }
                    if cpu == self.hart_id && generation == entry.generation =>
                {
                    scheduling.run_state = RunState::Running { cpu: self.hart_id };
                    drop(scheduling);
                    self.current = Some(entry.task.clone());
                    return Some(entry.task);
                }
                _ => {
                    // generation 不匹配说明 stop/continue 等转换已废弃该 entry，只消费不执行。
                }
            }
        }
    }

    /// @description 把远端 mailbox 中的任务转移到本 hart scheduler。
    ///
    /// @return 无返回值。
    pub fn drain_inbound_to_local(&mut self) {
        let slot = per_hart(self.hart_id);
        let mut inbound = slot.inbound.lock();
        let mut local = VecDeque::new();
        core::mem::swap(&mut *inbound, &mut local);
        drop(inbound);
        per_hart(self.hart_id)
            .inbound_entries
            .fetch_sub(local.len(), Ordering::Relaxed);
        for entry in local {
            self.add_ready_entry(entry);
        }
    }

    /// @description 发布当前 processor 已进入 idle/scheduler 循环。
    ///
    /// @return 无返回值。
    pub fn mark_active(&self) {
        // Release 发布 local Processor 初始化；远端负载均衡读取 active(Acquire) 后
        // 才能向 inbound 入队。缺失时会向尚未开始 drain 的 hart 投递任务。
        per_hart(self.hart_id).active.store(true, Ordering::Release);
    }

    fn defer_reap(&mut self, task: Arc<TaskControlBlock>) {
        assert!(
            self.deferred_reap.is_none(),
            "deferred reap slot must be drained before another task exits"
        );
        self.deferred_reap = Some(task);
    }

    fn take_deferred_reap(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.deferred_reap.take()
    }

    fn request_reschedule(&mut self) {
        self.need_reschedule = true;
    }

    fn take_reschedule(&mut self) -> bool {
        core::mem::take(&mut self.need_reschedule)
    }
}

struct PerHartProcessor {
    local: UnsafeCell<MaybeUninit<Processor>>,
    initialized: AtomicBool,
    active: AtomicBool,
    // 仅供跨 hart 负载选择；Relaxed 过期值只影响选择，不发布或拥有 runqueue entry。
    queued_entries: AtomicUsize,
    // 与 inbound mutex 内容器同步增减；Relaxed 读取只作为近似负载 hint。
    inbound_entries: AtomicUsize,
    // timer softirq 可远端投递 runnable task；IRQ-safe lock 防止打断本 hart drain 后再入。
    inbound: IrqMutex<VecDeque<RunQueueEntry>>,
}

impl PerHartProcessor {
    const fn new() -> Self {
        Self {
            local: UnsafeCell::new(MaybeUninit::uninit()),
            initialized: AtomicBool::new(false),
            active: AtomicBool::new(false),
            queued_entries: AtomicUsize::new(0),
            inbound_entries: AtomicUsize::new(0),
            inbound: IrqMutex::new(VecDeque::new()),
        }
    }
}

// SAFETY: `local` 只能由索引等于当前 hart_id 的执行流访问；远端 hart 只能触及
// active/queued_entries 原子和 inbound Mutex。trap 入口保持 SIE 关闭，因此同 hart 不会重入 local 可变借用。
unsafe impl Sync for PerHartProcessor {}

static PER_HART_PROCESSORS: [PerHartProcessor; MAX_CORES] = [
    const { PerHartProcessor::new() },
    const { PerHartProcessor::new() },
    const { PerHartProcessor::new() },
    const { PerHartProcessor::new() },
    const { PerHartProcessor::new() },
    const { PerHartProcessor::new() },
    const { PerHartProcessor::new() },
    const { PerHartProcessor::new() },
];

static NEXT_CPU: AtomicUsize = AtomicUsize::new(0);

#[inline(always)]
fn per_hart(hart: usize) -> &'static PerHartProcessor {
    PER_HART_PROCESSORS
        .get(hart)
        .expect("processor hart index exceeds MAX_CORES")
}

fn local_processor() -> &'static mut Processor {
    let hart = hart_id();
    let slot = per_hart(hart);
    // initialized 只由本 hart 在关闭 SIE 时读写，不承担跨 hart 发布；缺失该分支会重复构造 Processor。
    if !slot.initialized.load(Ordering::Relaxed) {
        // SAFETY: 只有 hart `hart` 能到达自己的 slot.local，且 S-mode trap 不开启嵌套中断。
        unsafe { (*slot.local.get()).write(Processor::new(hart)) };
        slot.initialized.store(true, Ordering::Relaxed);
    }
    // SAFETY: 与上面的 per-hart 唯一所有权约束相同，initialized 证明对象已构造。
    unsafe { (*slot.local.get()).assume_init_mut() }
}

/// @description 在关闭本地 S-mode 中断期间访问当前 hart 独占的 processor。
///
/// @param f 不得保存或泄漏 `Processor` 引用的同步闭包。
/// @return 闭包的返回值。
/// @errors `tp` 越界属于内核不变量破坏。
pub fn with_current_processor<R>(f: impl FnOnce(&mut Processor) -> R) -> R {
    let _irq = LocalIrqGuard::disable();
    // 中断关闭保证同 hart 的 trap handler 不能在该 mutable borrow 存活时再次借用 local processor。
    f(local_processor())
}

/// @description 将当前 exiting Task 在 task stack 上的 owner 移交给所属 hart。
///
/// @param task 必须是已从 current、PID index 与 runqueue 移除的退出任务。
/// @return 无返回值；slot 未先 drain 表示 terminal ownership 协议损坏并 panic。
pub(super) fn defer_task_reap(task: Arc<TaskControlBlock>) {
    with_current_processor(|processor| processor.defer_reap(task));
}

/// @description 在 idle stack 上取得并释放 deferred exiting Task。
///
/// @return slot 为空时不执行操作；存在任务时 deferred Arc 在本函数返回前于 idle stack Drop。
pub(super) fn reap_deferred_task() {
    let task = with_current_processor(Processor::take_deferred_reap);
    drop(task);
}

/// @description 标记当前 hart 在返回用户态前需要重新调度。
///
/// @return 无返回值；flag 仅由本 hart 在关中断临界区访问。
pub fn request_reschedule() {
    with_current_processor(Processor::request_reschedule);
}

/// @description 消费当前 hart 的 reschedule flag。
///
/// @return 本次用户态返回是否应先 yield。
pub fn take_reschedule() -> bool {
    with_current_processor(Processor::take_reschedule)
}

/// @description 将已完成 Ready transition 的 entry 投递给指定 active hart。
///
/// @param cpu_id 目标 hart ID。
/// @param entry 带 generation 的 membership token。
/// @return 无返回值。
/// @errors 目标越界、未 active 或 SBI IPI 失败均触发内核不变量失败，不做 CPU fallback。
fn deliver_ready_entry(cpu_id: usize, entry: RunQueueEntry) {
    assert!(
        cpu_id < MAX_CORES,
        "target CPU {} exceeds MAX_CORES",
        cpu_id
    );
    let current = hart_id();
    if cpu_id == current {
        with_current_processor(|processor| processor.add_ready_entry(entry));
        return;
    }

    let target = per_hart(cpu_id);
    assert!(
        target.active.load(Ordering::Acquire),
        "cannot enqueue task to inactive CPU {}",
        cpu_id
    );
    let mut inbound = target.inbound.lock();
    inbound.push_back(entry);
    target.inbound_entries.fetch_add(1, Ordering::Relaxed);
    drop(inbound);
    sbi::sbi_send_ipi(1usize << cpu_id, 0).expect("SBI IPI failed for task mailbox");
}

/// @description 在 active hart 中选择近似负载最低者。
///
/// @param task 只读取 last-CPU hint，不改变其状态。
/// @return 被选中的 hart ID。
fn select_cpu(task: &TaskControlBlock) -> usize {
    // Relaxed 只用于分散扫描起点，不承担任何状态发布。
    let start = NEXT_CPU.fetch_add(1, Ordering::Relaxed) % MAX_CORES;
    let current = hart_id();
    // last_cpu 仅提供缓存亲和性提示；过期值只影响候选顺序，不影响任务所有权或可见性。
    let last = task.scheduling.last_cpu.load(Ordering::Relaxed);
    let mut best_cpu = current;
    let mut best_load = usize::MAX;
    let mut last_load = None;

    for offset in 0..MAX_CORES {
        let cpu = (start + offset) % MAX_CORES;
        let slot = per_hart(cpu);
        if !slot.active.load(Ordering::Acquire) {
            continue;
        }
        let load = slot
            .queued_entries
            .load(Ordering::Relaxed)
            .saturating_add(slot.inbound_entries.load(Ordering::Relaxed));
        if load < best_load {
            best_load = load;
            best_cpu = cpu;
        }
        if cpu == last {
            last_load = Some(load);
        }
    }

    match last_load {
        Some(load) if load <= best_load.saturating_add(1) => last,
        _ => best_cpu,
    }
}

fn ready_entry(task: Arc<TaskControlBlock>, generation: u64) -> RunQueueEntry {
    let vruntime = task.scheduling.policy.lock().vruntime;
    RunQueueEntry {
        task,
        generation,
        vruntime,
    }
}

/// @description 将新建 Task 从 New 转换为唯一 Ready membership 并投递。
///
/// @param task TGID index 已拥有的初始 Task。
/// @return 选中的 CPU。
pub fn enqueue_new_task(task: Arc<TaskControlBlock>) -> usize {
    let cpu = select_cpu(&task);
    let generation = {
        let mut scheduling = task.scheduling.state.lock();
        assert_eq!(
            scheduling.run_state,
            RunState::New,
            "task must start in New"
        );
        scheduling.transition_to_ready(cpu)
    };
    deliver_ready_entry(cpu, ready_entry(task, generation));
    cpu
}

/// @description 消费一个明确 deadline wait membership，并完成无丢失唤醒转换。
///
/// @param task wait queue 移出的 task owner。
/// @param wait_key 必须与 SchedulingState 中记录的 key 相同。
/// @return 本次调用真正消费 membership 时返回 true；重复/stale wake 返回 false。
pub(super) fn wake_deadline_task(task: Arc<TaskControlBlock>, wait_key: (u64, u64)) -> bool {
    let target_cpu = select_cpu(&task);
    let ready = {
        let mut scheduling = task.scheduling.state.lock();
        if scheduling.deadline_wait != Some(wait_key) {
            return false;
        }
        scheduling.deadline_wait = None;
        match scheduling.run_state {
            RunState::Blocking { cpu } => {
                scheduling.run_state = RunState::WakePending { cpu };
                None
            }
            RunState::Blocked => {
                let generation = scheduling.transition_to_ready(target_cpu);
                Some((target_cpu, generation))
            }
            RunState::Exited => None,
            state => panic!("deadline wait attached to invalid state {state:?}"),
        }
    };
    if let Some((cpu, generation)) = ready {
        deliver_ready_entry(cpu, ready_entry(task, generation));
    }
    true
}

/// @description 在 idle stack 上完成 Blocking/WakePending 的切出握手。
///
/// @param task 刚从该 CPU 切回 idle 的 task。
/// @return 无返回值；WakePending 会直接加入本 CPU local runqueue。
pub(super) fn finish_blocking_transition(task: &Arc<TaskControlBlock>) {
    let cpu = hart_id();
    let generation = {
        let mut scheduling = task.scheduling.state.lock();
        match scheduling.run_state {
            RunState::Blocking { cpu: owner } => {
                assert_eq!(owner, cpu, "blocking task returned on another CPU");
                scheduling.run_state = RunState::Blocked;
                None
            }
            RunState::WakePending { cpu: owner } => {
                assert_eq!(owner, cpu, "wake-pending task returned on another CPU");
                Some(scheduling.transition_to_ready(cpu))
            }
            _ => None,
        }
    };
    if let Some(generation) = generation {
        with_current_processor(|processor| {
            processor.add_ready_entry(ready_entry(task.clone(), generation))
        });
    }
}
