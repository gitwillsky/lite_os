pub(crate) const INIT_PID: usize = 1;
pub(super) use crate::namespace::{
    PidIdentity, PidLinkSlot, PidRegistration, PreparedPid, RetiredPidRegistrations,
};

/// Process 的不可变 TGID identity pin；graph registration 独立拥有实际 namespace 编号占用。
#[derive(Debug)]
pub(super) struct ProcessId(alloc::sync::Arc<PidIdentity>);
impl ProcessId {
    pub(super) fn from_identity(identity: alloc::sync::Arc<PidIdentity>) -> Self {
        Self(identity)
    }
    pub(super) fn key(&self) -> usize {
        self.0.key()
    }
    pub(super) fn identity(&self) -> &alloc::sync::Arc<PidIdentity> {
        &self.0
    }
}
