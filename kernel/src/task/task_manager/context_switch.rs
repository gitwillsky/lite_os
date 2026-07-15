use super::*;
use crate::task::{context::TaskContext, with_current_processor};

/// @description 已发布唯一 wait membership、且外部 owner guard 已释放的单次阻塞转换。
pub(super) struct PreparedBlock {
    // Some 表示 current 已撤销但尚未切回 idle；若 token 被遗弃，task 将永久停在 Blocking。
    task: Option<Arc<TaskControlBlock>>,
}

impl PreparedBlock {
    /// @description 消费 prepared transition，切回 idle，并取得唯一 wake result。
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
/// @param task calling hart 当前唯一 Running task。
/// @param owner 覆盖 readiness/signal 复查到 membership publication 的外部 owner guard。
/// @param publish 在 scheduling lock 内向 owner 发布 waiter，并返回对应 membership。
/// @return owner guard 已释放、只允许执行一次 suspend 的 prepared transition。
/// @panics current、run state、既有 wait/result 或 runtime owner 不一致时 panic。
pub(super) fn prepare_current_block<Owner>(
    task: &Arc<TaskControlBlock>,
    mut owner: Owner,
    publish: impl FnOnce(&mut Owner, Arc<TaskControlBlock>) -> WaitMembership,
) -> PreparedBlock {
    let cpu = hart_id();
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

/// @description 保持 task owner 存活，安全地从 task context 切回当前 hart idle context。
///
/// @param task 即将让出 CPU 的当前 task；函数在切换前释放临时 Arc，避免它滞留在不再恢复的 task stack。
pub(super) fn schedule_with_task_context(task: Arc<TaskControlBlock>) {
    super::enforce_cpu_limit(&task);
    // 1. 只提取稳定 raw pointer，避免 `&mut Processor` 跨越会执行任意代码的 context switch。
    let idle_task_cx_ptr = with_current_processor(Processor::idle_context_ptr);
    let task_cx_ptr = {
        let mut task_cx = task.task_context().lock();
        let ptr = &mut *task_cx as *mut TaskContext;
        if ptr.is_null() {
            panic!("Task context pointer is null for task {}", task.tid());
        }
        ptr
    };

    // 2. TaskManager/runqueue/wait registry 保证 raw context 存活；先释放临时 Arc，
    //    否则 task 若不再恢复，该 Arc 会永久埋在自身 stack。
    drop(task);
    // 3. 两侧 context 均由稳定 owner 保留，且此时没有 scheduler guard 跨越切换。
    // SAFETY: task/idle owners retain both contexts until the switch completes.
    unsafe { crate::task::__switch(task_cx_ptr, idle_task_cx_ptr) }
}
