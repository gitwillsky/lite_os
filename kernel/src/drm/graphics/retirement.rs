//! OFD 退出只销毁 context；共享资源与关联的回收分别由设备的 UNREF/detach 队列拥有。
use crate::drivers::VirglCommand;

pub(in crate::drm) struct VirglCleanup {
    context_id: u32,
    in_flight: Option<u64>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::drm) enum VirglCleanupAction {
    Context { context_id: u32 },
}
pub(in crate::drm) enum VirglCleanupCompletion {
    Context,
}
impl VirglCleanupAction {
    pub(in crate::drm) const fn command(self) -> VirglCommand<'static> {
        let Self::Context { context_id } = self;
        VirglCommand::ContextDestroy { context_id }
    }
}
impl VirglCleanup {
    pub(super) fn new(context_id: u32) -> Self {
        Self {
            context_id,
            in_flight: None,
        }
    }
    pub(in crate::drm) fn next_action(&self, _completed: u64) -> Option<VirglCleanupAction> {
        self.in_flight
            .is_none()
            .then_some(VirglCleanupAction::Context {
                context_id: self.context_id,
            })
    }
    pub(in crate::drm) fn record_submission(&mut self, _action: VirglCleanupAction, fence: u64) {
        assert!(self.in_flight.replace(fence).is_none());
    }
    pub(in crate::drm) fn owns_fence(&self, fence: u64) -> bool {
        self.in_flight == Some(fence)
    }
    pub(in crate::drm) fn complete(&mut self, fence: u64) -> Option<VirglCleanupCompletion> {
        if self.in_flight != Some(fence) {
            return None;
        }
        self.in_flight = None;
        Some(VirglCleanupCompletion::Context)
    }
}
