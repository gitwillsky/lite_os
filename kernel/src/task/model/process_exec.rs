use super::*;

/// exec preparation 的映像错误与竞争；drain 提交后没有可恢复失败。
#[derive(Debug)]
pub(crate) enum ExecError {
    Image(ElfLoadError),
    Busy,
    Permission,
    Interrupted,
    FatalInterrupted,
    FatalImage,
}
impl From<ElfLoadError> for ExecError {
    fn from(error: ElfLoadError) -> Self {
        Self::Image(error)
    }
}
impl From<MemoryError> for ExecError {
    fn from(error: MemoryError) -> Self {
        Self::Image(error.into())
    }
}

/// 为 exec/task constructor 构造可失败的共享 owner。
///
/// # Parameters
///
/// - `value`: 尚未发布、失败时可直接析构的 owner value。
///
/// # Returns
///
/// Arc control block 分配成功时返回 owner；失败返回 ELF OutOfMemory。
pub(super) fn try_elf_arc<T>(value: T) -> Result<Arc<T>, ElfLoadError> {
    Arc::try_new(value).map_err(|_| ElfLoadError::OutOfMemory)
}

pub(super) fn process_name(path: &[u8]) -> [u8; 16] {
    let name = path
        .rsplit(|byte| *byte == b'/')
        .find(|component| !component.is_empty())
        .unwrap_or(path);
    let mut comm = [0; 16];
    let length = name.len().min(15);
    comm[..length].copy_from_slice(&name[..length]);
    comm
}

impl TaskControlBlock {
    /// exec/TSYNC 共用的实际 Process credential preparation guard。
    pub(in crate::task) fn credential_guard(&self) -> &TaskMutex<()> {
        &self.process().credential_guard
    }

    /// 完整准备新 ELF 映像，统一终止 sibling 后原子提交 calling Process。
    ///
    /// # Parameters
    ///
    /// - `loaded`: 已完成 pathname/script/ELF resolution 的 immutable exec input。
    /// - `envs`: 写入新用户栈的环境。
    ///
    /// # Returns
    ///
    /// 准备或提交成功返回 `Ok(())`；ELF/内存错误在修改 Process 前返回。
    ///
    /// # Errors
    ///
    /// 映像错误与内存不足返回 Image；并发 exec/group-exit 在 drain 前返回 Busy。
    pub(crate) fn execve_replace(
        &self,
        loaded: &LoadedExecutable,
        envs: &[Vec<u8>],
    ) -> Result<(), ExecError> {
        let preparation = super::super::process_table::ExecPreparation::claim(self)?;
        // 步骤1: 在不修改当前 Process 的前提下，完整准备新映像和初始栈。
        let metadata = loaded.credential_metadata();
        let credentials = self
            .prepare_exec_credentials(
                metadata.mode,
                metadata.uid,
                metadata.gid,
                loaded.file_capability(),
            )
            .map_err(|_| ExecError::Permission)?;
        let stack_limit = self.resource_limit(RLIMIT_STACK).unwrap().soft;
        let address_space_limit = self.resource_limit(RLIMIT_AS).unwrap().soft;
        let data_limit = self.resource_limit(RLIMIT_DATA).unwrap().soft;
        let exec_pid_link = if self.tid() != self.tgid() {
            Some(
                crate::task::pid::PidLinkSlot::try_new()
                    .map_err(|_| ExecError::Image(ElfLoadError::OutOfMemory))?,
            )
        } else {
            None
        };
        let exec_trace_link = super::super::process_table::ptrace::prepare_exec_link(self)?;
        let (new_memory_set, user_sp, entry_point) = loaded.build_address_space(
            envs,
            credentials.stack_credentials(),
            stack_limit,
            address_space_limit,
            data_limit,
        )?;
        let new_address_space = AddressSpace::new(new_memory_set, self.user_namespace())?;
        // 不可发布可 dump 的新mm；CLOEXEC/credential transaction 再按旧身份设置。
        new_address_space.set_dumpable(false);
        let prepared_context =
            new_address_space.prepare_context_rebind(&self.thread().user_context, TRAP_CONTEXT)?;
        let new_comm = process_name(loaded.execfn());

        preparation.terminate_siblings(self)?;
        // de_thread 后复制实时 files/sighand；不能恢复早期 snapshot，遗漏 sibling 最后修改。
        // 此时已越过 PONR，OOM 清理新映像后必须 fatal，不得返回旧 userspace。
        let detached_files = self
            .prepare_file_table_unshare()
            .map_err(|_| ExecError::FatalImage)?;
        let detached_handlers = self
            .prepare_exec_signal_handlers()
            .map_err(|_| ExecError::FatalImage)?;
        // 全部 preparation 完成后进入不可失败阶段。robust cleanup 使用旧 TID；
        // 随后的 graph promotion 才领取 TGID，并在映像提交前推进 exec epoch 阻止 parent setpgid。
        // Linux exec 在旧 mm 仍可访问时完成 robust owner-death publication，并清除
        // per-Thread registration；否则相同 VA 在新映像中会被误当成旧 robust list。
        self.cleanup_robust_list();
        super::super::process_table::mark_process_exec(self, exec_pid_link, exec_trace_link);
        // POSIX timers 不跨 exec 保留；在旧映像仍唯一运行、且提交已不可失败时清理，
        // 否则新程序会收到由旧 handler/value 创建的异步 signal。
        crate::task::remove_posix_timers_for_exec(self.tgid());
        self.enter_exec_time_namespace();

        // 步骤2: 单次替换 Process 映像相关状态；vfork child 只替换自己的 Process handle，
        // parent 与 sibling 继续持有旧 AddressSpace，因此不存在共享 handle 被原地清空的窗口。
        let kernel_stack_top = self.execution.kernel_stack.get_top();
        let old_context_binding = self.thread().user_context.binding();
        let old_address_space = self.replace_address_space(new_address_space);
        let retired_scheduler_alias = core::mem::replace(
            &mut *self.process().leader_scheduler.lock(),
            self.scheduling.clone(),
        );
        drop(retired_scheduler_alias);
        let retired_io_alias = core::mem::replace(
            &mut *self.process().leader_io.lock(),
            self.thread().io_accounting.clone(),
        );
        drop(retired_io_alias);
        let retired_mm_alias = core::mem::replace(
            &mut *self.process().leader_mm.lock(),
            self.thread().mm.clone(),
        );
        drop(retired_mm_alias);
        if let Some(prepared) = prepared_context {
            prepared.apply(&self.thread().user_context);
        }
        *self.thread().comm.lock() = new_comm;
        let retired_comm = core::mem::replace(
            &mut *self.process().leader_comm.lock(),
            self.thread().comm.clone(),
        );
        drop(retired_comm);
        self.commit_file_table_unshare(detached_files);
        let retired_files_alias = core::mem::replace(
            &mut *self.process().leader_files.lock(),
            self.thread().files.clone(),
        );
        drop(retired_files_alias);
        self.close_cloexec_files();
        self.commit_exec_signal_handlers(detached_handlers);
        self.reset_signal_stack_for_exec();
        self.clear_syscall_restart();
        let retired_identity = core::mem::replace(
            &mut *self.process().leader_credentials.lock(),
            self.thread().credentials.clone(),
        );
        drop(retired_identity);
        let retired_mask = core::mem::replace(
            &mut *self.process().leader_signal_mask.lock(),
            self.thread().signal_mask.clone(),
        );
        drop(retired_mask);
        let retired_namespaces_alias = core::mem::replace(
            &mut *self.process().leader_fs_namespaces.lock(),
            self.thread().fs_namespaces.clone(),
        );
        drop(retired_namespaces_alias);
        self.commit_exec_credentials(credentials, loaded.readable());

        // 步骤3: 参数与环境只存在于新初始栈；地址空间由统一 trap return 激活。
        self.replace_user_context(UserContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.wait().lock().kernel_trap_token(),
            kernel_stack_top,
            self.thread().kernel_trap_handler,
        ));
        // exec 不继承旧 image 的 live FP/NEON state；AArch64 在显式 asm boundary 清零，
        // RISC-V state 已由上面的新 UserContext image 覆盖。缺失该 hook 会跨 exec 泄漏寄存器。
        crate::arch::context::reset_live_floating_point();
        if old_context_binding.requires_retirement_wait(TRAP_CONTEXT) {
            // single-thread 只限定当前 Process；CLONE_VM parent/其他 Process 可合法占用旧 mm。
            // 复用 constructor 预置 waiter，PONR 后不能因 cleanup waiter OOM 失败或竞争 panic。
            self.with_cleanup_memory(&old_address_space, |memory| {
                memory.remove_thread_trap_context(old_context_binding.address());
            });
        }
        super::process_exit::release_exec_mm(
            self,
            old_address_space,
            self.take_clear_child_tid(),
            self.user_fault_limits(),
        );
        // vfork parent 只能在完整 exec commit 且 RISC-V child 临时 trap VMA 已删除后恢复；
        // AArch64 context 随独立 KernelStack 保活。提前唤醒会让共享旧 mm 的 detach 顺序失效。
        super::super::process_table::vfork::complete_vfork_exec(self.tid());
        super::super::process_table::ptrace::exec_completed(self);
        Ok(())
    }

    /// `prctl(PR_SET_NAME)`：仅替换 calling Thread 的 comm，遇 NUL 或 15 字节截断，不分配。
    pub(crate) fn set_comm(&self, name: &[u8]) {
        let length = name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(name.len())
            .min(15);
        let mut comm = [0; 16];
        comm[..length].copy_from_slice(&name[..length]);
        *self.thread().comm.lock() = comm;
    }

    /// `prctl(PR_GET_NAME)` 的 calling Thread 名称，固定 16 字节并以 NUL 填充。
    pub(crate) fn comm_buffer(&self) -> [u8; 16] {
        *self.thread().comm.lock()
    }

    /// get_task_mm 的真实 main ELF path；kernel/退出 Thread 无 mm，不读 sibling 或 zombie Process 的影子资源。
    pub(crate) fn process_executable(
        &self,
    ) -> Result<Option<Arc<crate::fs::OpenedFile>>, crate::fs::FileSystemError> {
        let Some(mm) = self.mm_snapshot() else {
            return Ok(None);
        };
        mm.executable()
    }
}

impl super::Process {
    /// 冻结 leader 的名称别名；释放名称锁后才分配输出，不 pin leader 执行资源。
    /// 输出分配失败返回 OutOfMemory，不改变名称。
    pub(super) fn comm_snapshot(&self) -> Result<Vec<u8>, MemoryError> {
        let owner = self.leader_comm.lock().clone();
        let comm = *owner.lock();
        let length = comm
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(comm.len());
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(length)
            .map_err(|_| MemoryError::OutOfMemory)?;
        snapshot.extend_from_slice(&comm[..length]);
        Ok(snapshot)
    }
}
