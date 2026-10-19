//! 以生产嵌套层级装配 policy owner，保持 task 域限定 visibility。
pub(crate) use super::TaskControlBlock;
// 每个 focused fixture 只消费部分 seam；完整 transaction 在 seccomp_tsync fixture 执行。
#[allow(dead_code)]
#[path = "../../../../tools/kernel-view/task/model/seccomp/seccomp/seccomp.rs"]
pub(crate) mod seccomp;

impl Clone for seccomp::ThreadSecurityOwner {
    fn clone(&self) -> Self {
        let child = Self::default();
        drop(child.inherit_from(self));
        child
    }
}

// These three fixtures exercise policy/graph/ABI, never descriptor injection. The actual request
// owner is exercised with real operations by seccomp_notifications, not these route stubs.
impl super::TaskControlBlock {
    fn fd_get(&self, _: usize) -> Option<alloc::sync::Arc<crate::file::OpenFileDescription>> {
        unreachable!("descriptor lookup outside policy-only fixture")
    }
    fn fd_receive(
        &self,
        _: alloc::sync::Arc<crate::file::OpenFileDescription>,
        _: Option<usize>,
        _: bool,
    ) -> Result<usize, crate::file::FileDescriptorError> {
        unreachable!("descriptor injection outside policy-only fixture")
    }
    #[allow(
        dead_code,
        reason = "listener publication is exercised by TSYNC and notification fixtures"
    )]
    pub(crate) fn fd_prepare_open(
        &self,
        _: bool,
    ) -> Result<crate::seccomp_environment::file::Reservation, crate::file::FileDescriptorError>
    {
        unreachable!("listener publication outside policy-only fixture")
    }
}
