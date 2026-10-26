//! OFD 退出时的资源/上下文回收状态机；只消费 exact completion，不拥有 adapter。
use super::VirglBuffer;
use crate::{
    drivers::VirglCommand,
    fallible_tree::{FallibleMap, VacantEntry},
};
use alloc::sync::Arc;

/// DRM OFD 关闭后由 device completion owner 串行推进的 VirGL 回收队列。
pub(in crate::drm) struct VirglCleanup {
    context_id: u32,
    pub(super) buffers: FallibleMap<u32, Arc<VirglBuffer>>,
    in_flight: Option<(u64, VirglCleanupAction)>,
}

/// 一次 OFD 回收中当前可提交的标准 VirtIO-GPU operation。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::drm) enum VirglCleanupAction {
    Resource { handle: u32, resource_id: u32 },
    Context { context_id: u32 },
}

pub(in crate::drm) enum VirglCleanupCompletion {
    Resource(VacantEntry<u32, Arc<VirglBuffer>>),
    Context,
}

impl VirglCleanupAction {
    /// 降低为不包含 kernel owner 的标准 adapter command。
    pub(in crate::drm) const fn command(self) -> VirglCommand<'static> {
        match self {
            Self::Resource { resource_id, .. } => VirglCommand::ResourceUnref { resource_id },
            Self::Context { context_id } => VirglCommand::ContextDestroy { context_id },
        }
    }
}

impl VirglCleanup {
    pub(super) fn new(context_id: u32) -> Self {
        Self {
            context_id,
            buffers: FallibleMap::new(),
            in_flight: None,
        }
    }

    /// 在 adapter 空闲时选择一个 fence 已完成的 resource，全部释放后销毁 context。
    pub(in crate::drm) fn next_action(&self, completed: u64) -> Option<VirglCleanupAction> {
        if self.in_flight.is_some() {
            return None;
        }
        if let Some((&handle, buffer)) = self
            .buffers
            .iter()
            .find(|(_, buffer)| buffer.lifecycle.lock().fence <= completed)
        {
            return Some(VirglCleanupAction::Resource {
                handle,
                resource_id: buffer.resource_id,
            });
        }
        self.buffers
            .is_empty()
            .then_some(VirglCleanupAction::Context {
                context_id: self.context_id,
            })
    }

    /// 记录已由 adapter 接受的唯一回收 operation 与 exact fence。
    pub(in crate::drm) fn record_submission(&mut self, action: VirglCleanupAction, fence: u64) {
        assert!(self.in_flight.replace((fence, action)).is_none());
    }

    /// 判断 completion fence 是否属于本 cleanup queue 的当前 operation。
    pub(in crate::drm) fn owns_fence(&self, fence: u64) -> bool {
        self.in_flight
            .is_some_and(|(expected, _)| expected == fence)
    }

    /// 消费 exact completion，将回收节点交给调用者在 device 锁外析构。
    pub(in crate::drm) fn complete(&mut self, fence: u64) -> Option<VirglCleanupCompletion> {
        let (expected, action) = self.in_flight?;
        if expected != fence {
            return None;
        }
        self.in_flight = None;
        match action {
            VirglCleanupAction::Resource { handle, .. } => Some(VirglCleanupCompletion::Resource(
                self.buffers
                    .take_entry(&handle)
                    .expect("submitted VirGL cleanup resource disappeared"),
            )),
            VirglCleanupAction::Context { .. } => {
                assert!(self.buffers.is_empty());
                Some(VirglCleanupCompletion::Context)
            }
        }
    }
}
