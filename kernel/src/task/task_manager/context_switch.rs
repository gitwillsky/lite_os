use super::*;
use crate::{
    arch::context::KernelContext,
    cpu,
    sync::LocalIrqGuard,
    task::{
        Processor,
        processor::{
            finish_deschedule_transition, publish_pending_handoff, resume_without_switch,
            take_pending_handoff,
        },
        with_current_processor,
    },
};
use core::sync::atomic::Ordering;

/// @description 已发布唯一 wait membership、且外部 owner guard 已释放的单次阻塞转换。
pub(super) struct PreparedBlock {
    // Some 表示 current 已撤销但尚未进入 scheduler handoff；若 token 被遗弃，task 将永久停在 Blocking。
    task: Option<Arc<TaskControlBlock>>,
}

impl PreparedBlock {
    /// @description 消费 prepared transition，直接 handoff 给 successor，并取得唯一 wake result。
    ///
    /// @return waker 发布的 ready、timeout 或 signal interruption 结果。
    /// @panics membership 未被唯一 waker 消费，或 task 未重新调度时 panic。
    pub(super) fn suspend(mut self) -> WaitResult {
        let task = self
            .task
            .take()
            .expect("prepared block transition consumed twice");
        schedule_with_task_context(task.clone());
        task.scheduling
            .state
            .lock()
            .wait_result
            .take()
            .expect("blocked task resumed without a wake result")
    }
}

impl Drop for PreparedBlock {
    fn drop(&mut self) {
        assert!(
            self.task.is_none(),
            "prepared block transition dropped before context switch"
        );
    }
}

/// @description 将 current Running task 原子转换为拥有唯一 wait membership 的 Blocking task。
///
/// @param task calling CPU 当前唯一 Running task。
/// @param owner 覆盖 readiness/signal 复查到 membership publication 的外部 owner guard。
/// @param publish 在 scheduling lock 内向 owner 发布 waiter，并返回对应 membership。
/// @return owner guard 已释放、只允许执行一次 suspend 的 prepared transition。
/// @panics current、run state、既有 wait/result 或 runtime owner 不一致时 panic。
pub(super) fn prepare_current_block<Owner>(
    task: &Arc<TaskControlBlock>,
    mut owner: Owner,
    publish: impl FnOnce(&mut Owner, Arc<TaskControlBlock>) -> WaitMembership,
) -> PreparedBlock {
    let cpu = cpu::current_id();
    // 1. 只有确认需要阻塞后才结束 active slice，early return 不会重复提交 runtime。
    task.scheduling.policy.lock().finish_runtime(get_time_us());
    // 2. 外部 owner → processor → scheduling 是全部 wait publication 共用的锁序。
    with_current_processor(|processor| {
        let current = processor
            .take_current()
            .expect("blocking transition requires current task");
        assert!(Arc::ptr_eq(&current, task));
        let mut scheduling = task.scheduling.state.lock();
        assert_eq!(scheduling.run_state(), RunState::Running { cpu });
        assert!(scheduling.wait.is_none());
        assert!(scheduling.wait_result.is_none());
        scheduling.wait = Some(publish(&mut owner, current));
        scheduling.replace_non_ready_state(RunState::Blocking { cpu });
    });
    // 3. token 返回前强制释放 owner；若 guard 跨 context switch，waker 会永久死锁。
    drop(owner);
    PreparedBlock {
        task: Some(task.clone()),
    }
}

/// @description 保持 task owner 存活，优先直接切到 next runnable task；无 successor 才切 idle。
///
/// @param task 即将让出 CPU 的当前 task；切换前移交给 per-CPU pending handoff slot 保活。
pub(super) fn schedule_with_task_context(task: Arc<TaskControlBlock>) {
    super::enforce_cpu_limit(&task);
    // 1. IRQ closed 覆盖 deferred drain、successor selection 与 pending consequence
    // publication；guard 由 switch target continuation 消费，不跨 CPU。
    let handoff_irq = LocalIrqGuard::disable();
    super::scheduler_deferred_safe_point();
    with_current_processor(|processor| processor.drain_inbound_to_local());
    let next_task_cx_ptr = select_task_switch_target();

    // 当前 task 仍是唯一 runnable owner 且没有竞争者时直接恢复 Running；进入 idle 再
    // 选择自己会制造两次无意义 context switch。
    if next_task_cx_ptr.is_none() && resume_without_switch(&task) {
        begin_task_runtime(&task);
        drop(handoff_irq);
        return;
    }

    let task_cx_ptr = {
        let mut kernel_cx = task.kernel_context().lock();
        let ptr = &mut *kernel_cx as *mut KernelContext;
        if ptr.is_null() {
            panic!("Task context pointer is null for task {}", task.tid());
        }
        ptr
    };
    let target_cx_ptr = next_task_cx_ptr.unwrap_or_else(|| {
        with_current_processor(Processor::idle_context_ptr) as *const KernelContext
    });

    // 2. outgoing consequence 只有在 assembly 已保存 task context 后才能发布 Ready/Blocked；
    // 提前发布会允许远端 CPU 恢复仍在执行的 stack。next 或 idle continuation 唯一消费。
    publish_pending_handoff(task, handoff_irq);
    // SAFETY: pending handoff 保活 save target；Processor current 或 idle owner 保活 restore target。
    unsafe { crate::arch::context::switch_kernel_context(task_cx_ptr, target_cx_ptr) }
    complete_pending_handoff();
}

/// @description 在 idle context 中恢复已经由 Processor 选中的 task。
/// @param task `Processor.current` 唯一保活的 Running owner。
/// @return 仅在其他 task 恢复 idle context 时返回。
pub(super) fn switch_from_idle(task: Arc<TaskControlBlock>) {
    let idle_task_cx_ptr = with_current_processor(Processor::idle_context_ptr);
    let next_task_cx_ptr = begin_task_runtime(&task);
    // SAFETY: idle context 由当前 CPU 独占，Processor current 与 task Arc 保活 next context。
    unsafe { crate::arch::context::switch_kernel_context(idle_task_cx_ptr, next_task_cx_ptr) }
    // 恢复 idle 的 task 可能不是最初由这个 frame dispatch 的 task；唯一 pending slot
    // 记录真实 outgoing owner，因此不得使用上面的局部 `task` 完成 transition。
    complete_pending_handoff();
    crate::task::processor::reap_deferred_task();
}

fn select_task_switch_target() -> Option<*const KernelContext> {
    let task = with_current_processor(Processor::select_task)?;
    Some(begin_task_runtime(&task))
}

fn begin_task_runtime(task: &Arc<TaskControlBlock>) -> *const KernelContext {
    let cpu = cpu::current_id();
    with_current_processor(|processor| {
        let current = processor
            .current
            .as_ref()
            .expect("selected task missing from current");
        assert!(
            Arc::ptr_eq(current, task),
            "selected task differs from current"
        );
    });
    assert_eq!(
        task.scheduling.state.lock().run_state(),
        RunState::Running { cpu },
        "selected task must be Running on this CPU"
    );
    task.scheduling.policy.lock().begin_runtime(get_time_us());
    task.scheduling
        .last_cpu
        .store(cpu.index(), Ordering::Relaxed);
    let kernel_cx = task.kernel_context().lock();
    &*kernel_cx as *const KernelContext
}

/// @description 在 next task 或 idle stack 上完成前一次 outgoing scheduling consequence。
/// @return 没有 pending consequence 时不执行操作。
pub(in crate::task) fn complete_pending_handoff() {
    let Some((task, irq)) = take_pending_handoff() else {
        return;
    };
    if finish_deschedule_transition(&task) {
        super::complete_process_stop(task.tgid());
    }
    drop(task);
    drop(irq);
}
