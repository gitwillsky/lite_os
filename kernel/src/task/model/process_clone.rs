use super::*;

impl TaskControlBlock {
    /// @description 以 COW 用户页构造 fork child，Process 级非内存资源仍独立复制。
    /// @param pid TaskManager 已唯一分配、尚未发布的 child TGID/TID。
    /// @return 成功返回尚处于 New 状态的 child；OOM 时 parent 完全不变。
    pub(in crate::task) fn fork_process(&self, pid: ProcessId) -> Result<Self, MemoryError> {
        self.clone_process(pid, false, 0)
    }

    /// @description 以同一 AddressSpace 构造尚未发布的 vfork child。
    /// @param pid TaskManager 已唯一分配的 child TGID/TID。
    /// @param child_stack 非零时覆盖 child SP；零值继承 parent SP。
    /// @return 成功返回 New child；parent 必须在发布后阻塞到 child exec/exit。
    /// @errors 地址空间或 Process 资源分配失败时返回 MemoryError。
    pub(in crate::task) fn vfork_process(
        &self,
        pid: ProcessId,
        child_stack: usize,
    ) -> Result<Self, MemoryError> {
        self.clone_process(pid, true, child_stack)
    }

    fn clone_process(
        &self,
        pid: ProcessId,
        share_user_memory: bool,
        child_stack: usize,
    ) -> Result<Self, MemoryError> {
        // share_user_memory 只区分 fork COW 与 vfork CLONE_VM contract；缺失该选择会让
        // posix_spawn child 的 stack/errno-pipe 操作脱离 parent mm，破坏标准 handoff。
        let tid = pid.0;
        // 1. 先构造地址空间和所有可能失败的 process-owned 资源，发布前不修改 process graph。
        let parent_address_space = self.process.address_space();
        let address_space = if share_user_memory {
            parent_address_space.clone()
        } else {
            let memory_set = parent_address_space
                .memory_set
                .lock()
                .try_clone_for_fork()?;
            AddressSpace::new(memory_set)?
        };
        let cwd = self.process.cwd.lock().clone();
        let files = self
            .process
            .files
            .lock()
            .try_clone()
            .map_err(|_| MemoryError::OutOfMemory)?;
        let signal_actions = self.process.signal_state.lock().actions;
        let credentials = self.process.credentials.lock().clone();
        let resource_limits = self.process.resource_limits.lock().forked();
        let kernel_stack = KernelStack::try_new()?;
        let kernel_stack_top = kernel_stack.get_top();
        let cpu_runtime_us = Arc::try_new(core::sync::atomic::AtomicU64::new(0))
            .map_err(|_| MemoryError::OutOfMemory)?;
        let io_accounting =
            Arc::try_new(IoAccounting::default()).map_err(|_| MemoryError::OutOfMemory)?;
        let child_policy = self.scheduling.policy.lock().forked(cpu_runtime_us.clone());
        let last_cpu = self
            .scheduling
            .last_cpu
            .load(core::sync::atomic::Ordering::Relaxed);
        let cpu_affinity = self.scheduling.state.lock().cpu_affinity;
        let alternate_signal_stack = *self.thread.alternate_signal_stack.lock();
        let start_time_us = get_time_us();
        let parent_comm = self.process.comm.lock();
        let mut comm = Vec::new();
        comm.try_reserve_exact(parent_comm.len())
            .map_err(|_| MemoryError::OutOfMemory)?;
        comm.extend_from_slice(&parent_comm);
        drop(parent_comm);
        let process = Arc::try_new(Process {
            tgid: pid,
            comm: Mutex::new(comm),
            start_time_us,
            address_space: Mutex::new(address_space.clone()),
            cwd: Mutex::new(cwd),
            files: Mutex::new(files),
            credentials: Mutex::new(credentials),
            resource_limits: Mutex::new(resource_limits),
            cpu_runtime_us: cpu_runtime_us.clone(),
            io_accounting: io_accounting.clone(),
            terminal: Mutex::new(self.process.terminal.lock().clone()),
            signal_state: Mutex::new(ProcessSignalState::new(signal_actions)),
        })
        .map_err(|_| MemoryError::OutOfMemory)?;
        // 2. vfork child 在共享 mm 中使用按全局 TID 分配的 supervisor trap page；若复用
        // spawning Thread 的页，仍在运行的 sibling 或 parent 恢复现场会被 child 覆盖。
        // 该分配放在所有其他 fallible preparation 之后，保证失败不残留 shared-mm VMA。
        let user_cx_va = if share_user_memory {
            address_space
                .memory_set
                .lock()
                .allocate_thread_trap_context(tid)?
        } else {
            TRAP_CONTEXT
        };

        // 3. child 从同一条已前移 syscall PC 返回，但 a0 必须为零且使用自己的 kernel stack。
        let mut child_trap = self.load_user_context();
        child_trap
            .prepare_process_clone((child_stack != 0).then_some(child_stack), kernel_stack_top);
        let child = Self {
            process,
            thread: ThreadContext {
                tid,
                start_time_us,
                kernel_stack,
                user_cx_va: Mutex::new(user_cx_va),
                kernel_cx: Mutex::new(KernelContext::goto_trap_return(
                    kernel_stack_top,
                    self.thread.kernel_trap_return,
                )),
                kernel_trap_handler: self.thread.kernel_trap_handler,
                kernel_trap_return: self.thread.kernel_trap_return,
                clear_child_tid: Mutex::new(None),
                robust_list: Mutex::new(None),
                signal_mask: Mutex::new(*self.thread.signal_mask.lock()),
                pending_signals: Mutex::new(PendingSignals::new()),
                suspend_restore_mask: Mutex::new(None),
                syscall_restart: Mutex::new(None),
                parent_death: Mutex::new(ParentDeathState::default()),
                alternate_signal_stack: Mutex::new(alternate_signal_stack),
                io_accounting: IoAccounting::default(),
            },
            scheduling: SchedulingEntity {
                state: IrqMutex::new(SchedulingState::new(cpu_affinity)),
                policy: Mutex::new(child_policy),
                last_cpu: AtomicUsize::new(last_cpu),
            },
        };
        child.set_user_context(child_trap);
        Ok(child)
    }
}
