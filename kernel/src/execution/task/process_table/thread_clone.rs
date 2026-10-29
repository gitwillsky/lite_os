use super::*;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ThreadCloneError {
    Memory(crate::memory::MemoryError),
    ResourceLimit,
    Pid(crate::namespace::PidAllocationError),
    PidFd(super::pidfd::PidFdError),
}

/// 在当前 thread group 内创建共享 Process 资源的新 Thread。
///
/// # Parameters
///
/// - `stack`: 16-byte aligned child 用户栈顶。
/// - `tls`: child `tp`。
/// - `parent_tid`: 可选 parent TID copyout。
/// - `child_set_tid`: 可选 child TID copyout。
/// - `clear_child_tid`: 可选 thread exit 清零地址。
///
/// # Returns
///
/// 成功返回 child TID；任何验证/分配失败都不发布 graph/runqueue membership。
pub(crate) fn clone_current_thread(
    parameters: crate::execution::task::CloneParameters,
    resources: crate::execution::task::ThreadCloneResources,
) -> Result<usize, ThreadCloneError> {
    let parent = current_task().expect("thread clone requires current task");
    let namespace = resources.namespaces.pid_for_children();
    if !Arc::ptr_eq(&parent.pid_namespace(), &namespace) {
        return Err(ThreadCloneError::Pid(
            crate::namespace::PidAllocationError::Invalid,
        ));
    }
    let mut pid = PROCESS_TABLE
        .allocate_pid(
            namespace,
            &parameters.requested_pids,
            &parent.namespace_authority(),
        )
        .map_err(ThreadCloneError::Pid)?;
    let tid = pid.identity().key();
    let graph_slot = FallibleMap::<usize, Arc<TaskControlBlock>>::try_reserve_node()
        .map_err(|_| ThreadCloneError::Memory(crate::memory::MemoryError::OutOfMemory))?;
    let thread_index_slot = FallibleMap::<usize, ThreadIndex>::try_reserve_node()
        .map_err(|_| ThreadCloneError::Memory(crate::memory::MemoryError::OutOfMemory))?;
    let child = try_allocate_task(
        ThreadCloneError::Memory(crate::memory::MemoryError::OutOfMemory),
        || {
            parent
                .clone_thread(pid.identity(), &parameters, &resources)
                .map_err(ThreadCloneError::Memory)
        },
    )?;
    drop(resources);
    // 文件表已复制/共享之后才预留 parent pidfd；非 CLONE_FILES 的 child 不继承它。
    let prepared_pidfd = match parameters
        .pidfd
        .map(|address| {
            super::pidfd::PreparedPidFd::for_clone(pid.identity().clone(), true, &parent, address)
        })
        .transpose()
    {
        Ok(prepared) => prepared,
        Err(error) => {
            child.remove_thread_trap_context();
            return Err(ThreadCloneError::PidFd(error));
        }
    };
    let mut minimum_snapshot_capacity = 0;
    let mut snapshot = match ThreadSlotSnapshot::prepare(minimum_snapshot_capacity) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            child.remove_thread_trap_context();
            return Err(ThreadCloneError::Memory(error));
        }
    };
    loop {
        let creation = PROCESS_TABLE.process_creation.lock();
        if let Err(required) = snapshot.capture() {
            drop(creation);
            minimum_snapshot_capacity = required;
            snapshot = match ThreadSlotSnapshot::prepare(minimum_snapshot_capacity) {
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
        let thread_index = thread_index_slot.fill(
            tid,
            ThreadIndex {
                pid: pid.take_registration(),
                pidfd_source: prepared_pidfd
                    .as_ref()
                    .map_or_else(alloc::sync::Weak::new, |prepared| prepared.source()),
                tgid: parent.tgid(),
                exit_in_progress: false,
                retiring_task: None,
                created_children: FallibleMap::new(),
                vfork_parent: None,
            },
        );
        let result = PROCESS_TABLE.publish_thread(
            &parent,
            child.clone(),
            membership,
            thread_index,
            &mut pid,
        );
        drop(creation);
        match result {
            Ok(retired) => drop(retired),
            Err(error) => {
                child.remove_thread_trap_context();
                return Err(ThreadCloneError::Pid(error));
            }
        }
        break;
    }
    drop(snapshot);
    if let Some(prepared) = prepared_pidfd {
        prepared.publish();
    }
    parent.write_clone_tid_values(
        [parameters.parent_set_tid, None],
        child.visible_tid() as i32,
    );
    child.write_clone_tid_values([parameters.child_set_tid, None], child.visible_tid() as i32);
    drop(parameters);
    let visible = child.visible_tid() as usize;
    PROCESS_TABLE.activate_thread(parent.tgid(), child);
    Ok(visible)
}
