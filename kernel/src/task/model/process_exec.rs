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
        let stack_limit = self.resource_limit(RLIMIT_STACK).unwrap().soft;
        let address_space_limit = self.resource_limit(RLIMIT_AS).unwrap().soft;
        let data_limit = self.resource_limit(RLIMIT_DATA).unwrap().soft;
        let (new_memory_set, user_sp, entry_point) =
            loaded.build_address_space(envs, stack_limit, address_space_limit, data_limit)?;
        let new_address_space = AddressSpace::new(new_memory_set)?;
        let new_comm = process_name(loaded.execfn());
        let credential_metadata = loaded.credential_metadata();

        // exec 准备完成后进入不可失败的提交阶段；先发布 has_execed，才能与 parent setpgid
        // 在 process graph lock 上建立确定顺序，避免新映像已经生效而 parent 仍错误改组。
        super::super::task_manager::mark_process_exec(self.tgid());

        // 步骤2: 单次替换 Process 映像相关状态；vfork child 只替换自己的 Process handle，
        // parent 与 sibling 继续持有旧 AddressSpace，因此不存在共享 handle 被原地清空的窗口。
        let kernel_stack_top = self.thread.kernel_stack.get_top();
        let old_trap_context = self.trap_context_va();
        let old_address_space = self.process.replace_address_space(new_address_space);
        *self.process.comm.lock() = new_comm;
        *self.thread.trap_cx_va.lock() = TRAP_CONTEXT;
        self.close_cloexec_files();
        self.process
            .signal_state
            .lock()
            .reset_dispositions_for_exec();
        self.reset_signal_stack_for_exec();
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
        if old_trap_context != TRAP_CONTEXT {
            old_address_space
                .memory_set
                .lock()
                .remove_thread_trap_context(old_trap_context);
        }
        // vfork parent 只能在完整 exec commit 且 child 临时 trap page 已从共享 mm 删除后恢复；
        // 提前唤醒会让 parent 与尚未 detach 的 child 并发修改同一 AddressSpace。
        super::super::task_manager::vfork::complete_vfork_exec(self.tgid());
        Ok(())
    }
}
