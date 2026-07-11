use crate::sync::{IrqMutex, LocalIrqGuard};
use crate::{
    arch::{
        hart::{MAX_CORES, hart_id},
        sbi,
    },
    task::{
        TaskControlBlock,
        context::TaskContext,
        scheduler::{Scheduler, cfs_scheduler::CFScheduler},
    },
};
use alloc::{boxed::Box, collections::VecDeque, sync::Arc};
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
    scheduler: Box<dyn Scheduler>,
}

impl Processor {
    fn new(hart_id: usize) -> Self {
        let mut idle_context = TaskContext::zero_init();
        idle_context.set_ra(idle_return as usize);
        Self {
            hart_id,
            current: None,
            idle_context,
            scheduler: Box::new(CFScheduler::new()),
        }
    }

    /// @description 获取当前 hart idle context 的稳定地址。
    ///
    /// @return 指向本 hart `TaskContext` 的唯一可变指针。
    pub fn idle_context_ptr(&mut self) -> *mut TaskContext {
        &mut self.idle_context
    }

    /// @description 把 ready task 加入当前 hart 的本地调度队列。
    ///
    /// @param task 当前 hart 独占转移进 scheduler 的任务引用。
    /// @return 无返回值。
    pub fn add_task(&mut self, task: Arc<TaskControlBlock>) {
        self.scheduler.add_task(task);
        // queued_tasks 只提供负载估计；scheduler membership 由 owner hart 和 inbound lock 保护。
        per_hart(self.hart_id)
            .queued_tasks
            .fetch_add(1, Ordering::Relaxed);
    }

    /// @description 从当前 hart 的本地调度队列取一个任务。
    ///
    /// @return 队列为空时返回 `None`，否则返回唯一取出的任务引用。
    pub fn fetch_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        let task = self.scheduler.fetch_task()?;
        per_hart(self.hart_id)
            .queued_tasks
            .fetch_sub(1, Ordering::Relaxed);
        Some(task)
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
        for task in local {
            self.add_task(task);
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
}

struct PerHartProcessor {
    local: UnsafeCell<MaybeUninit<Processor>>,
    initialized: AtomicBool,
    active: AtomicBool,
    queued_tasks: AtomicUsize,
    // timer softirq 可远端投递 runnable task；IRQ-safe lock 防止打断本 hart drain 后再入。
    inbound: IrqMutex<VecDeque<Arc<TaskControlBlock>>>,
}

impl PerHartProcessor {
    const fn new() -> Self {
        Self {
            local: UnsafeCell::new(MaybeUninit::uninit()),
            initialized: AtomicBool::new(false),
            active: AtomicBool::new(false),
            queued_tasks: AtomicUsize::new(0),
            inbound: IrqMutex::new(VecDeque::new()),
        }
    }
}

// SAFETY: `local` 只能由索引等于当前 hart_id 的执行流访问；远端 hart 只能触及
// active/queued_tasks 原子和 inbound Mutex。trap 入口保持 SIE 关闭，因此同 hart 不会重入 local 可变借用。
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

/// @description 将任务投递给指定 active hart。
///
/// @param cpu_id 目标 hart ID。
/// @param task 待投递任务。
/// @return 无返回值。
/// @errors 目标越界、未 active 或 SBI IPI 失败均触发内核不变量失败，不做 CPU fallback。
pub fn add_task_to_cpu(cpu_id: usize, task: Arc<TaskControlBlock>) {
    assert!(
        cpu_id < MAX_CORES,
        "target CPU {} exceeds MAX_CORES",
        cpu_id
    );
    let current = hart_id();
    if cpu_id == current {
        with_current_processor(|processor| processor.add_task(task));
        return;
    }

    let target = per_hart(cpu_id);
    assert!(
        target.active.load(Ordering::Acquire),
        "cannot enqueue task to inactive CPU {}",
        cpu_id
    );
    target.inbound.lock().push_back(task);
    sbi::sbi_send_ipi(1usize << cpu_id, 0).expect("SBI IPI failed for task mailbox");
}

/// @description 在 active hart 中选择负载最低者并投递任务。
///
/// @param task 待投递任务。
/// @return 被选中的 hart ID。
pub fn add_task_to_best_cpu(task: Arc<TaskControlBlock>) -> usize {
    // Relaxed 只用于分散扫描起点，不承担任何状态发布。
    let start = NEXT_CPU.fetch_add(1, Ordering::Relaxed) % MAX_CORES;
    let current = hart_id();
    // last_cpu 仅提供缓存亲和性提示；过期值只影响候选顺序，不影响任务所有权或可见性。
    let last = task.last_cpu.load(Ordering::Relaxed);
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
            .queued_tasks
            .load(Ordering::Relaxed)
            .saturating_add(slot.inbound.lock().len());
        if load < best_load {
            best_load = load;
            best_cpu = cpu;
        }
        if cpu == last {
            last_load = Some(load);
        }
    }

    let chosen = match last_load {
        Some(load) if load <= best_load.saturating_add(1) => last,
        _ => best_cpu,
    };
    add_task_to_cpu(chosen, task);
    chosen
}
