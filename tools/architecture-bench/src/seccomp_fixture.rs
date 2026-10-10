//! 按生产嵌套层级装配真实 policy owner；benchmark 只计 evaluation，不外推 graph 提交成本。
pub(crate) use super::TaskControlBlock;
#[path = "../../../kernel/src/task/model/seccomp.rs"]
pub(crate) mod seccomp;

pub(crate) fn install_filter(owner: &seccomp::ThreadSecurityOwner, bytes: &[u8]) {
    let validated = seccomp::ValidatedFilter::prepare(bytes, 0).expect("valid benchmark filter");
    let prepared = owner
        .prepare_filter(&validated)
        .expect("benchmark filter preparation");
    assert!(prepared.matches(owner));
    drop(prepared.publish(owner, false));
}

impl super::TaskControlBlock {
    fn tid_identity(&self) -> alloc::sync::Arc<crate::namespace::PidIdentity> {
        unreachable!("benchmark only evaluates filters")
    }
    fn fd_get(&self, _: usize) -> Option<alloc::sync::Arc<crate::file::OpenFileDescription>> {
        unreachable!("benchmark only evaluates filters")
    }
    fn fd_receive(
        &self,
        _: alloc::sync::Arc<crate::file::OpenFileDescription>,
        _: Option<usize>,
        _: bool,
    ) -> Result<usize, crate::file::FileDescriptorError> {
        unreachable!("benchmark only evaluates filters")
    }
}
