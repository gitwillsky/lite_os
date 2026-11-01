use super::*;

/// clone/clone3 解码后共享的执行参数；raw flag 和 user ABI layout 不进入 task。
pub(crate) struct CloneParameters {
    pub(crate) stack: usize,
    pub(crate) tls: Option<usize>,
    pub(crate) parent_set_tid: Option<usize>,
    /// CLONE_PIDFD 的独立 copyout；必须成功后才发布 child，不能沿用 best-effort TID copy。
    pub(crate) pidfd: Option<usize>,
    pub(crate) child_set_tid: Option<usize>,
    pub(crate) clear_child_tid: Option<usize>,
    /// checkpoint restore 的 leaf→ancestor 精确编号；空序列使用正常 cyclic allocation。
    pub(crate) requested_pids: Vec<i32>,
}

/// Process clone 的语义资源选择；syscall 拥有 raw Linux flag 校验。
pub(crate) struct ProcessCloneResources {
    /// CLONE_PARENT 在最终 graph transaction 选择 caller 的实际 real parent。
    pub(crate) inherit_parent: bool,
    /// 独立进程的 native exit notification；零不发信号，仍保留可等待 zombie。
    pub(crate) exit_signal: u8,
    pub(crate) share_memory: bool,
    pub(crate) share_fs: bool,
    pub(crate) share_files: bool,
    pub(crate) share_sem_undo: bool,
    pub(crate) share_handlers: bool,
    pub(crate) clear_handlers: bool,
    pub(crate) wait_for_child: bool,
    pub(crate) namespaces: super::namespaces::PreparedCloneNamespaces,
}

impl TaskControlBlock {
    /// 根据 Linux clone resource bits 准备尚未发布的独立 Process。
    ///
    /// # Parameters
    ///
    /// - `pid`: graph 唯一分配、尚未发布的 TGID/TID。
    /// - `child_stack`: 非零时覆盖 child SP；零值继承 caller SP。
    /// - `resources`: VM/FS/FILES/SIGHAND 共享与 VFORK 完成等待选择。
    ///
    /// # Errors
    ///
    /// 任一 preparation OOM 不发布 graph；共享 mm 的 trap VMA 在所有前置分配之后准备。
    pub(in crate::execution::task) fn clone_process(
        &self,
        pid: ProcessId,
        parameters: &CloneParameters,
        resources: &ProcessCloneResources,
    ) -> Result<Self, MemoryError> {
        let identity = pid.identity().clone();
        // 1. 先构造地址空间和所有可能失败的 process-owned 资源，发布前不修改 process graph。
        let parent_address_space = self.address_space();
        let address_space = if resources.share_memory {
            parent_address_space.clone()
        } else {
            parent_address_space.try_clone_for_fork()?
        };
        let mm = try_mm_handle(address_space.clone())?;
        let fs_context = resources.namespaces.fs_context(self, resources.share_fs)?;
        let namespaces = resources
            .namespaces
            .handle(fs_context, resources.share_memory)?;
        let files = self.clone_file_table(resources.share_files)?;
        let file_wait =
            TaskMutexWaitPreparation::prepare().map_err(|_| MemoryError::OutOfMemory)?;
        let sem_undo = self
            .prepare_semaphore_undo(resources.share_sem_undo)
            .map_err(|_| MemoryError::OutOfMemory)?;
        let signal_handlers =
            self.clone_signal_handlers(resources.share_handlers, resources.clear_handlers)?;
        let mut inherited_credentials = self.thread().credentials.lock().clone();
        if let Some(user) = resources.namespaces.user() {
            inherited_credentials.cloned_namespace(user);
        }
        let credentials = Arc::try_new(Mutex::new(inherited_credentials))
            .map_err(|_| MemoryError::OutOfMemory)?;
        let resource_limits = self.process().resource_limits.lock().forked();
        let cpu_limit_active = resource_limits.cpu_limit_active();
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
        let scheduling = SchedulingEntity::try_new(child_policy, cpu_affinity, last_cpu)
            .map_err(|_| MemoryError::OutOfMemory)?;
        let alternate_signal_stack = if resources.share_memory && !resources.wait_for_child {
            AlternateSignalStack::disabled()
        } else {
            *self.thread().alternate_signal_stack.lock()
        };
        let start_time_us = get_time_us();
        let comm =
            Arc::try_new(Mutex::new(self.comm_buffer())).map_err(|_| MemoryError::OutOfMemory)?;
        let thread_io =
            Arc::try_new(IoAccounting::default()).map_err(|_| MemoryError::OutOfMemory)?;
        let leader_credentials =
            Arc::try_new(Mutex::new(credentials.clone())).map_err(|_| MemoryError::OutOfMemory)?;
        let signal_mask = Arc::try_new(Mutex::new(*self.thread().signal_mask.lock()))
            .map_err(|_| MemoryError::OutOfMemory)?;
        let process = Arc::try_new(Process {
            tgid: pid,
            leader_comm: Mutex::new(comm.clone()),
            start_time_us,
            leader_mm: Mutex::new(mm.clone()),
            leader_scheduler: Mutex::new(scheduling.clone()),
            leader_io: Mutex::new(thread_io.clone()),
            leader_files: Mutex::new(files.clone()),
            leader_credentials,
            leader_signal_mask: Mutex::new(signal_mask.clone()),
            leader_fs_namespaces: Mutex::new(namespaces.clone()),
            credential_guard: TaskMutex::new(()),
            retirements: Arc::try_new(ThreadRetirements::new())
                .map_err(|_| MemoryError::OutOfMemory)?,
            resource_limits: Arc::try_new(Mutex::new(resource_limits))
                .map_err(|_| MemoryError::OutOfMemory)?,
            cpu_limit_active: core::sync::atomic::AtomicBool::new(cpu_limit_active),
            cpu_runtime_us: cpu_runtime_us.clone(),
            usage: UsageAccounting::default(),
            reaped_children: ReapedChildrenUsage::default(),
            io_accounting: io_accounting.clone(),
            // controlling TTY 在 graph publication 与 force/setsid 同锁捕获，不能继承准备期旧值。
            terminal: Mutex::new(None),
            signal_state: Mutex::new(ProcessSignalState::new(signal_handlers)),
        })
        .map_err(|_| MemoryError::OutOfMemory)?;
        // 2. CLONE_VM child 在共享 mm 中使用实际 MM 空闲位分配的 supervisor trap page；若复用
        // spawning Thread 的页，仍在运行的 sibling 或 parent 恢复现场会被 child 覆盖。
        // 该分配放在所有其他 fallible preparation 之后，保证失败不残留 shared-mm VMA。
        let kernel_context_address = kernel_stack.user_context_address();
        let memory_cleanup_wait =
            Some(TaskMutexWaitPreparation::prepare().map_err(|_| MemoryError::OutOfMemory)?);
        let user_context = if resources.share_memory && kernel_context_address.is_none() {
            address_space.allocate_thread_context()?
        } else {
            address_space.bind_user_context(ContextBinding::for_placement(
                kernel_context_address,
                TRAP_CONTEXT,
            ))?
        };

        // 3. child 从同一条已前移 syscall PC 返回，但 a0 必须为零且使用自己的 kernel stack。
        let mut child_trap = self.snapshot_user_context_for_clone();
        child_trap.prepare_clone(
            (parameters.stack != 0).then_some(parameters.stack),
            parameters.tls,
            kernel_stack_top,
        );
        let child = Self {
            kind: TaskKind::User {
                process,
                thread: ThreadContext {
                    comm,
                    mm,
                    user_context,
                    kernel_trap_handler: self.thread().kernel_trap_handler,
                    kernel_trap_return: self.thread().kernel_trap_return,
                    memory_cleanup_wait: Mutex::new(memory_cleanup_wait),
                    clear_child_tid: Mutex::new(parameters.clear_child_tid),
                    robust_list: Mutex::new(None),
                    sem_undo: Mutex::new(Some(sem_undo)),
                    shm_creator: Mutex::new(None),
                    signal_mask,
                    pending_signals: Mutex::new(PendingSignals::new()),
                    suspend_restore_mask: Mutex::new(None),
                    ptrace_resume: Mutex::new(None),
                    syscall_restart: Mutex::new(SyscallRestartState::default()),
                    parent_death: Mutex::new(ParentDeathState::default()),
                    // security 只在 graph 原子 publication 时继承真实 creator 的最新状态。
                    security: ThreadSecurityOwner::default(),
                    credentials,
                    fs_namespaces: namespaces,
                    files,
                    file_wait: Mutex::new(Some(file_wait)),
                    alternate_signal_stack: Mutex::new(alternate_signal_stack),
                    io_accounting: thread_io,
                    usage: UsageAccounting::default(),
                },
            },
            execution: ExecutionContext {
                identity: Mutex::new(identity),
                start_time_us,
                kernel_stack,
                kernel_cx: Mutex::new(KernelContext::clone_for_trap_return(
                    kernel_stack_top,
                    crate::execution::task::resume_new_task,
                )),
            },
            scheduling,
        };
        child.replace_user_context(child_trap);
        Ok(child)
    }
}
