use super::*;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ThreadCloneError {
    Memory(crate::memory::MemoryError),
    Fault,
    ResourceLimit,
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
    let creation = TASK_MANAGER.process_creation.lock();
    if !check_process_slot() {
        return Err(ThreadCloneError::ResourceLimit);
    }
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
    drop(creation);
    Ok(tid)
}
