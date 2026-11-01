use super::{CapabilityError, TaskControlBlock};
use syscall_abi::capability::{Capability, CapabilitySets, CapabilitySnapshot};

impl TaskControlBlock {
    /// 读取 calling Thread 同一 credential owner 的 effective capability。
    pub(crate) fn has_capability(&self, capability: Capability) -> bool {
        self.thread()
            .credentials
            .lock()
            .capabilities
            .capable(capability)
    }
    /// 具名 target user namespace 的 commoncap 权限；child capability 不授权 ancestor。
    pub(crate) fn has_namespace_capability(
        &self,
        target: &alloc::sync::Arc<crate::namespace::UserNamespace>,
        capability: Capability,
    ) -> bool {
        self.namespace_authority().capable(target, capability)
    }

    /// 全局 kernel/hardware resource 显式要求 initial namespace，而不是 calling user namespace。
    pub(crate) fn has_initial_capability(&self, capability: Capability) -> bool {
        self.has_namespace_capability(&crate::namespace::initial_user(), capability)
    }

    /// 无分配取得三个 capability sets 与 bounding/ambient 的同锁只读投影。
    pub(crate) fn capability_snapshot(&self) -> CapabilitySnapshot {
        self.thread().credentials.lock().capabilities.snapshot()
    }
    /// Process 与 graph 只读共享同一 retargetable leader alias；不保活 TCB/mm/files。
    pub(in crate::task) fn leader_credentials_handle(&self) -> super::LeaderCredentialsHandle {
        self.process().leader_credentials.clone()
    }
    /// 只更新 calling Thread；capset 校验全部成功后一次提交。
    pub(crate) fn set_capabilities(&self, sets: CapabilitySets) -> Result<(), CapabilityError> {
        self.thread().credentials.lock().capabilities.set(sets)
    }
    /// capability prctl 与 UID/exec 共用同一 credential lock，不另设 privilege flag。
    pub(crate) fn capability_prctl(
        &self,
        option: u32,
        argument: usize,
        remaining: [usize; 3],
    ) -> Result<usize, CapabilityError> {
        self.thread()
            .credentials
            .lock()
            .capabilities
            .prctl(option, argument, remaining)
    }
}
