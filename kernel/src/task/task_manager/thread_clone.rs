use super::*;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ThreadCloneError {
    Memory(crate::memory::MemoryError),
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
    let parent = current_task().expect("thread clone requires current task");
    let tid = TASK_MANAGER
        .allocate_pid()
        .ok_or(ThreadCloneError::ResourceLimit)?
        .0;
    let graph_slot = FallibleMap::<usize, Arc<TaskControlBlock>>::try_reserve_node()
        .map_err(|_| ThreadCloneError::Memory(crate::memory::MemoryError::OutOfMemory))?;
    let child = try_allocate_task(
        ThreadCloneError::Memory(crate::memory::MemoryError::OutOfMemory),
        || {
            parent
                .clone_thread(tid, stack, tls, clear_child_tid)
                .map_err(ThreadCloneError::Memory)
        },
    )?;
    let mut minimum_snapshot_capacity = 0;
    let mut snapshot = match ProcessSlotSnapshot::prepare(minimum_snapshot_capacity) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            child.remove_thread_trap_context();
            return Err(ThreadCloneError::Memory(error));
        }
    };
    loop {
        let creation = TASK_MANAGER.process_creation.lock();
        if let Err(required) = snapshot.capture() {
            drop(creation);
            minimum_snapshot_capacity = required;
            snapshot = match ProcessSlotSnapshot::prepare(minimum_snapshot_capacity) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    child.remove_thread_trap_context();
                    return Err(ThreadCloneError::Memory(error));
                }
            };
            continue;
        }
        if !snapshot.allows_current() {
            drop(creation);
            child.remove_thread_trap_context();
            return Err(ThreadCloneError::ResourceLimit);
        }
        let membership = graph_slot.fill(tid, child.clone());
        TASK_MANAGER.publish_thread(parent.tgid(), child.clone(), membership);
        drop(creation);
        break;
    }
    drop(snapshot);
    parent.write_clone_tid_values([parent_tid, child_set_tid], tid as i32);
    TASK_MANAGER.activate_thread(parent.tgid(), child);
    Ok(tid)
}
