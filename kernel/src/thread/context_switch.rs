//! 阻塞、让出与上下文切换。
//!
//! outgoing 线程的状态后果（Blocked/Ready）只能在其上下文已被汇编保存之后发布，否则远端 CPU
//! 可能恢复一个仍在执行的栈。因此切换前把 outgoing owner 放进 per-CPU pending handoff，
//! 由 next 线程或 idle continuation 唯一完成。

use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use super::Thread;
use super::processor::{
    self, Processor, finish_deschedule, publish_pending_handoff, resume_without_switch,
    take_pending_handoff, with_current_processor,
};
use super::scheduling::{RunState, WaitMembership};
use crate::arch::context::KernelContext;
use crate::execution::cpu;
use crate::execution::sync::{LocalIrqGuard, WaitResult};
use crate::execution::timer::get_time_ns;

/// 已发布唯一等待身份、尚未切出的阻塞转换。
#[must_use = "prepared block must be suspended"]
pub(super) struct PreparedBlock {
    // Some 表示 current 已撤销但尚未切出；被遗弃会让线程永久停在 Blocking。
    thread: Option<Arc<Thread>>,
}

impl PreparedBlock {
    /// 切出并在被唤醒后返回唤醒结果。
    pub(super) fn suspend(mut self) -> WaitResult {
        let thread = self.thread.take().expect("prepared block consumed twice");
        schedule(thread.clone());
        thread
            .scheduling
            .state
            .lock()
            .wait_result
            .take()
            .expect("blocked thread resumed without a wake result")
    }
}

impl Drop for PreparedBlock {
    fn drop(&mut self) {
        assert!(
            self.thread.is_none(),
            "prepared block dropped before context switch"
        );
    }
}

/// 把 current Running 线程原子转为带 `membership` 的 Blocking。
pub(super) fn prepare_block(thread: &Arc<Thread>, membership: WaitMembership) -> PreparedBlock {
    let cpu = cpu::current_id();
    thread.scheduling.policy.lock().finish(get_time_ns);
    with_current_processor(|processor| {
        let current = processor
            .take_current()
            .expect("blocking requires current thread");
        assert!(Arc::ptr_eq(&current, thread));
        let mut scheduling = thread.scheduling.state.lock();
        assert_eq!(scheduling.run_state(), RunState::Running { cpu });
        assert!(scheduling.wait.is_none() && scheduling.wait_result.is_none());
        scheduling.wait = Some(membership);
        scheduling.replace_non_ready_state(RunState::Blocking { cpu });
    });
    PreparedBlock {
        thread: Some(thread.clone()),
    }
}

/// 当前线程让出 CPU（被抢占或主动 yield）。
pub(crate) fn yield_current() {
    yield_with_budget(false);
}

/// 显式用户 yield：公平类让出，截止类放弃本周期剩余额度；不混同 timer 抢占。
pub(crate) fn yield_current_period() {
    yield_with_budget(true);
}

fn yield_with_budget(forfeit: bool) {
    let Some(thread) = super::current() else {
        return;
    };
    {
        let mut policy = thread.scheduling.policy.lock();
        policy.finish(get_time_ns);
        if forfeit {
            policy.forfeit_deadline();
        }
    }
    processor::begin_preempt_current(&thread);
    schedule(thread);
}

/// outgoing 线程已撤销 current：优先直接切到 next 线程，没有 successor 才切 idle。
fn schedule(thread: Arc<Thread>) {
    // 关中断覆盖 deferred drain、successor 选择与 pending handoff 发布；guard 由切换目标消费。
    let handoff_irq = LocalIrqGuard::disable();
    super::dispatch_deferred();
    with_current_processor(Processor::drain_inbound);
    let next = select_switch_target();
    if next.is_none() && resume_without_switch(&thread) {
        begin_runtime(&thread);
        // resume 清除旧请求后重新观察分区；否则并发隔离的 IPI 可能被该清除覆盖。
        processor::observe_partition();
        drop(handoff_irq);
        return;
    }
    let save = {
        let mut context = thread.kernel_context.lock();
        &mut *context as *mut KernelContext
    };
    let restore = next.unwrap_or_else(|| {
        with_current_processor(Processor::idle_context_ptr) as *const KernelContext
    });
    publish_pending_handoff(thread, handoff_irq);
    // SAFETY: pending handoff 保活 save 目标；current 或 idle owner 保活 restore 目标。
    unsafe { crate::arch::context::switch_kernel_context(save, restore) }
    complete_pending_handoff();
}

/// 在 idle 上下文中恢复已被选中的线程。
pub(super) fn switch_from_idle(thread: Arc<Thread>) {
    let idle = with_current_processor(Processor::idle_context_ptr);
    let next = begin_runtime(&thread);
    // SAFETY: idle 上下文由当前 CPU 独占；current 与 `thread` 保活 next 上下文。
    unsafe { crate::arch::context::switch_kernel_context(idle, next) }
    // 恢复 idle 的线程未必是本 frame 派发的那个；唯一 pending slot 记录真实 outgoing owner。
    complete_pending_handoff();
    processor::reap_deferred();
}

fn select_switch_target() -> Option<*const KernelContext> {
    let thread = with_current_processor(Processor::select)?;
    Some(begin_runtime(&thread))
}

fn begin_runtime(thread: &Arc<Thread>) -> *const KernelContext {
    let cpu = cpu::current_id();
    assert_eq!(
        thread.scheduling.state.lock().run_state(),
        RunState::Running { cpu },
        "selected thread must be Running on this CPU"
    );
    thread.scheduling.policy.lock().begin(get_time_ns);
    // 新时间片从现在开始：记录起点，并按本 CPU 是否有竞争者决定是否武装截止；
    // 未提交区间的起点同时发布给运行时间观察者。
    crate::execution::timer::note_slice_start();
    with_current_processor(|processor| processor::rearm_slice(processor));
    thread
        .scheduling
        .last_cpu
        .store(cpu.index(), Ordering::Relaxed);
    let context = thread.kernel_context.lock();
    &*context as *const KernelContext
}

/// 在 next 线程或 idle 栈上完成前一次切换的 outgoing 后果。
pub(super) fn complete_pending_handoff() {
    if let Some((thread, irq)) = take_pending_handoff() {
        finish_deschedule(&thread);
        drop(thread);
        processor::observe_partition();
        drop(irq);
    } else {
        processor::observe_partition();
    }
}

impl Thread {
    /// 创建尚未发表的内核工作线程；函数返回后走统一 exit/退休路径。
    ///
    /// # Parameters
    /// `entry` 是无捕获、可阻塞的工作函数；须按预算主动让出处理器，不借用启动栈数据。
    /// # Errors
    /// 栈、调度容量或 Arc 分配失败返回 `Err(())`，不留下已发表执行体。
    pub(crate) fn create_kernel(entry: fn()) -> Result<Arc<Self>, ()> {
        let kernel_stack = crate::vm::KernelStack::try_new()?;
        let top = kernel_stack.get_top();
        let scheduler_slot = processor::ThreadSlot::try_new()?;
        let thread = Arc::try_new(Self {
            id: scheduler_slot.id(),
            body: super::ThreadBody::Kernel(entry),
            _kernel_stack: kernel_stack,
            _scheduler_slot: scheduler_slot,
            kernel_context: spin::Mutex::new(KernelContext::with_resume(top, run_kernel_thread)),
            scheduling: super::scheduling::SchedulingEntity::new(
                super::PRIORITY_LEVELS - 1,
                0,
                cpu::current_id().index(),
            ),
            flags: core::sync::atomic::AtomicU32::new(0),
        })
        .map_err(|_| ())?;
        processor::register_thread(&thread);
        Ok(thread)
    }
}

fn run_kernel_thread() -> ! {
    complete_pending_handoff();
    // SAFETY: 能被调度的 CPU 已安装 trap vector/controller；新 continuation 从 IRQ-masked
    // switch 恢复，handoff 已完成且不再持有调度锁。内核工作函数必须允许设备中断推进。
    unsafe { crate::arch::interrupt::enable_scheduler_interrupts() };
    let entry = {
        let thread = super::current().expect("kernel continuation lost current owner");
        match &thread.body {
            super::ThreadBody::Kernel(entry) => *entry,
            super::ThreadBody::User(_) => panic!("user body entered kernel continuation"),
        }
    };
    entry();
    super::exit_current()
}
