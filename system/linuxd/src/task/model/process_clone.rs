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
    /// 任一 preparation OOM 不发布 graph。
    pub(in crate::task) fn clone_process(
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
            // 子地址空间的成员进程沿用父线程名（Linux fork 复制 comm），供内核诊断辨认。
            let comm = self.comm_buffer();
            let length = comm.iter().position(|&byte| byte == 0).unwrap_or(comm.len());
            parent_address_space.try_clone_for_fork(&comm[..length])?
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
        let retired_runtime = Arc::try_new(Mutex::new(super::usage_accounting::CpuUsage::default()))
            .map_err(|_| MemoryError::OutOfMemory)?;
        let io_accounting =
            Arc::try_new(IoAccounting::default()).map_err(|_| MemoryError::OutOfMemory)?;
        let child_policy = self.scheduling.policy.lock().forked();
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
            cpu_limit_active: super::resource_limits::CpuLimitActivity::new(cpu_limit_active),
            retired_runtime: retired_runtime.clone(),
            usage: UsageAccounting::default(),
            reaped_children: ReapedChildrenUsage::default(),
            io_accounting: io_accounting.clone(),
            // controlling TTY 在 graph publication 与 force/setsid 同锁捕获，不能继承准备期旧值。
            terminal: Mutex::new(None),
            signal_state: Mutex::new(ProcessSignalState::new(signal_handlers)),
        })
        .map_err(|_| MemoryError::OutOfMemory)?;
        let memory_cleanup_wait =
            Some(TaskMutexWaitPreparation::prepare().map_err(|_| MemoryError::OutOfMemory)?);
        // 2. child 从同一条已前移的 syscall PC 返回，返回值为零；浮点状态继承调用线程此刻的硬件状态。
        let mut child_trap = self.snapshot_user_context_for_clone();
        child_trap.prepare_clone((parameters.stack != 0).then_some(parameters.stack), parameters.tls);
        let user_context = UserContextCell::new(child_trap, crate::arch::fp::capture());
        let prepared_carrier = crate::trap::prepare_user_carrier(address_space.process())?;
        let child = Self {
            kind: TaskKind::User {
                process,
                thread: ThreadContext {
                    comm,
                    mm,
                    user_context,
                    restricted: Mutex::new(None),
                    cpu_time_retired: core::sync::atomic::AtomicBool::new(false),
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
                carrier: Mutex::new(None),
                prepared_carrier: Mutex::new(Some(prepared_carrier)),
            },
            scheduling,
        };
        Ok(child)
    }
}
