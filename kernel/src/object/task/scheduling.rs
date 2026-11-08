//! 线程对象的调度 façade；配置和动态预算仅由实际执行体拥有。
use super::{Execution, Thread};
use native_abi::Status;

impl Thread {
    /// 设置权重等级；未启动的线程在启动时应用。
    ///
    /// # Errors
    /// 等级越界返回 `ERR_INVALID_ARGS`；已退出返回 `ERR_BAD_STATE`。
    pub(crate) fn set_priority(&self, priority: u8) -> Result<(), Status> {
        if priority >= crate::thread::PRIORITY_LEVELS {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        match &mut *self.execution.lock() {
            Execution::Created(pending) => pending.priority = Some(priority),
            Execution::Running(thread) => thread
                .set_priority(priority)
                .map_err(|()| native_abi::ERR_INVALID_ARGS)?,
            Execution::Exited => return Err(native_abi::ERR_BAD_STATE),
        }
        Ok(())
    }

    /// 限制可运行的 CPU；未启动的线程在启动时应用。
    ///
    /// # Errors
    /// 集合与 possible CPU 不相交返回 `ERR_INVALID_ARGS`；已退出返回 `ERR_BAD_STATE`。
    pub(crate) fn set_affinity(&self, cpus: crate::execution::cpu::CpuSet) -> Result<(), Status> {
        if (cpus & crate::execution::cpu::possible()).is_empty() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let running = match &mut *self.execution.lock() {
            Execution::Created(pending) => {
                pending.affinity = Some(cpus);
                None
            }
            Execution::Running(thread) => Some(thread.clone()),
            Execution::Exited => return Err(native_abi::ERR_BAD_STATE),
        };
        // 迁移在执行状态锁外进行：调度器锁不嵌套在对象锁内。
        if let Some(thread) = running {
            thread
                .set_affinity(cpus)
                .map_err(|()| native_abi::ERR_INVALID_ARGS)?;
        }
        Ok(())
    }

    /// 替换已启动执行体的截止预算；未启动/退出返回 BAD_STATE，准入失败返回 NO_RESOURCES。
    /// 配置与凭据归执行体策略所有，不在对象层复制。
    pub(crate) fn set_deadline(
        &self,
        parameters: Option<crate::thread::DeadlineParameters>,
    ) -> Result<(), Status> {
        self.execution()
            .ok_or(native_abi::ERR_BAD_STATE)?
            .set_deadline(parameters)
            .map_err(|error| match error {
                crate::thread::DeadlineError::Capacity => native_abi::ERR_NO_RESOURCES,
                crate::thread::DeadlineError::Terminated => native_abi::ERR_BAD_STATE,
            })
    }

    /// 已消耗的 CPU 时间。
    pub(crate) fn runtime(&self) -> crate::thread::Runtime {
        self.execution()
            .map_or_else(Default::default, |thread| thread.runtime())
    }
}
