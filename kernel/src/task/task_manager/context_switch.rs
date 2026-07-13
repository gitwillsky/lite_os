use super::*;
use crate::task::{context::TaskContext, with_current_processor};

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
