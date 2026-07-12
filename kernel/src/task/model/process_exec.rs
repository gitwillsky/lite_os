use super::*;

pub(super) fn process_name(path: &[u8]) -> Vec<u8> {
    path.rsplit(|byte| *byte == b'/')
        .find(|component| !component.is_empty())
        .unwrap_or(path)
        .iter()
        .copied()
        .take(15)
        .collect()
}

impl TaskControlBlock {
    /// @description 原子准备并提交当前单线程 Process 的新 ELF 映像。
    ///
    /// @param loaded 已完成 pathname/script/ELF resolution 的 immutable exec input。
    /// @param envs 写入新用户栈的环境。
    /// @return 准备或提交成功返回 `Ok(())`；ELF/内存错误在修改 Process 前返回。
    /// @errors 不支持的 ELF 与内存不足分别映射为 `ElfLoadError`。
    pub(crate) fn execve_replace(
        &self,
        loaded: &LoadedExecutable,
        envs: &[Vec<u8>],
    ) -> Result<(), ElfLoadError> {
        // 步骤1: 在不修改当前 Process 的前提下，完整准备新映像和初始栈。
        let (new_memory_set, user_sp, entry_point) = loaded.build_address_space(envs)?;
        let new_comm = process_name(loaded.execfn());
        let credential_metadata = loaded.credential_metadata();

        // exec 准备完成后进入不可失败的提交阶段；先发布 has_execed，才能与 parent setpgid
        // 在 process graph lock 上建立确定顺序，避免新映像已经生效而 parent 仍错误改组。
        super::super::task_manager::mark_process_exec(self.tgid());

        // 步骤2: 单次替换 Process 映像相关状态；旧 MemorySet 不暴露 stale PTE 窗口。
        let kernel_stack_top = self.thread.kernel_stack.get_top();
        *self.process.address_space.memory_set.lock() = new_memory_set;
        // vfork parent 只能在 child 已脱离共享 user frame 后恢复；若在 has_execed 发布时
        // 提前唤醒，两个 Process 会并发写同一 stack/frame，违反 Linux vfork contract。
        super::super::task_manager::vfork::complete_vfork_exec(self.tgid());
        *self.process.comm.lock() = new_comm;
        *self.thread.trap_cx_va.lock() = TRAP_CONTEXT;
        self.process.files.lock().close_cloexec();
        self.process
            .signal_state
            .lock()
            .reset_dispositions_for_exec();
        self.apply_exec_setid(
            credential_metadata.mode,
            credential_metadata.uid,
            credential_metadata.gid,
        );

        // 步骤3: 参数与环境只存在于新初始栈；地址空间由统一 trap return 激活。
        self.set_trap_context(TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.wait().lock().token(),
            kernel_stack_top,
            self.thread.kernel_trap_handler,
        ));
        Ok(())
    }
}
