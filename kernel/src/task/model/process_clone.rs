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
        let (nice, vruntime) = {
            let policy = self.scheduling.policy.lock();
            (policy.nice, policy.vruntime)
        };
        let last_cpu = self
            .scheduling
            .last_cpu
            .load(core::sync::atomic::Ordering::Relaxed);
        let cpu_runtime_us = Arc::new(core::sync::atomic::AtomicU64::new(0));

        // 2. vfork child 在共享 mm 中使用按全局 TID 分配的 supervisor trap page；若复用
        // spawning Thread 的页，仍在运行的 sibling 或 parent 恢复现场会被 child 覆盖。
        // 该分配放在所有其他 fallible preparation 之后，保证失败不残留 shared-mm VMA。
        let trap_cx_va = if share_user_memory {
            address_space
                .memory_set
                .lock()
                .allocate_thread_trap_context(tid)?
        } else {
            TRAP_CONTEXT
        };

        // 3. child 从同一条已前移 syscall PC 返回，但 a0 必须为零且使用自己的 kernel stack。
        let mut child_trap = self.load_trap_context();
        child_trap.x[10] = 0;
        if child_stack != 0 {
            child_trap.set_sp(child_stack);
        }
        child_trap.kernel_sp = kernel_stack_top;
        child_trap.kernel_hart_id = 0;
        child_trap.kernel_gp = 0;
        let child = Self {
            process: Arc::new(Process {
                tgid: pid,
                comm: Mutex::new(self.process.comm.lock().clone()),
                start_time_us: get_time_us(),
                address_space: Mutex::new(address_space),
                cwd: Mutex::new(cwd),
                files: Mutex::new(files),
                credentials: Mutex::new(credentials),
                resource_limits: Mutex::new(resource_limits),
                cpu_runtime_us: cpu_runtime_us.clone(),
                terminal: self.process.terminal.clone(),
                signal_state: Mutex::new(ProcessSignalState::new(signal_actions)),
            }),
            thread: ThreadContext {
                tid,
                kernel_stack,
                trap_cx_va: Mutex::new(trap_cx_va),
                task_cx: Mutex::new(TaskContext::goto_trap_return(
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
            },
            scheduling: SchedulingEntity {
                state: IrqMutex::new(SchedulingState {
                    run_state: RunState::New,
                    next_generation: 0,
                    wait: None,
                    wait_result: None,
                }),
                policy: Mutex::new(Sched {
                    last_runtime: 0,
                    nice,
                    vruntime,
                    total_runtime_us: 0,
                    process_runtime_us: cpu_runtime_us,
                }),
                last_cpu: AtomicUsize::new(last_cpu),
            },
        };
        child.set_trap_context(child_trap);
        Ok(child)
    }
}
