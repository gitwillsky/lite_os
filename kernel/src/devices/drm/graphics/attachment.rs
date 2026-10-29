//! 每个 DRM context 的资源关联；GEM handle 与提交 pins 共享它，dma-buf 只保活资源本身。
use super::VirglBuffer;
use crate::{
    devices::drm::{DrmDevice, DrmError},
    fallible_tree::{FallibleMap, VacantEntry},
};
use alloc::sync::{Arc, Weak};
use spin::Mutex;

pub(in crate::devices::drm) struct AttachmentRetirement {
    pub(in crate::devices::drm) context_id: u32,
    pub(in crate::devices::drm) resource_id: u32,
    pub(in crate::devices::drm) buffer: Option<Arc<VirglBuffer>>,
    pub(in crate::devices::drm) wait_fence: u64,
    pub(in crate::devices::drm) in_flight: Option<u64>,
}

pub(in crate::devices::drm) struct VirglAttachment {
    pub(in crate::devices::drm) buffer: Arc<VirglBuffer>,
    device: Weak<DrmDevice>,
    // OWNER: 节点预分配但不反向引用 attachment；0 fence 表示 host 从未接受 attach，失败不发 detach。
    retirement: Mutex<Option<VacantEntry<(u32, u32), AttachmentRetirement>>>,
}
impl core::ops::Deref for VirglAttachment {
    type Target = VirglBuffer;
    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}
impl VirglAttachment {
    pub(in crate::devices::drm) fn prepare(
        device: &Arc<DrmDevice>,
        context_id: u32,
        buffer: Arc<VirglBuffer>,
    ) -> Result<Arc<Self>, DrmError> {
        let retirement = FallibleMap::try_prepare(
            (context_id, buffer.resource_id),
            AttachmentRetirement {
                context_id,
                resource_id: buffer.resource_id,
                buffer: None,
                wait_fence: 0,
                in_flight: None,
            },
        )
        .map_err(|_| DrmError::OutOfMemory)?;
        Arc::try_new(Self {
            buffer,
            device: Arc::downgrade(device),
            retirement: Mutex::new(Some(retirement)),
        })
        .map_err(|_| DrmError::OutOfMemory)
    }
    pub(in crate::devices::drm) fn arm(&self, fence: u64) {
        self.retirement
            .lock()
            .as_mut()
            .expect("live attachment lost retirement")
            .value_mut()
            .wait_fence = fence;
    }
}
impl Drop for VirglAttachment {
    fn drop(&mut self) {
        let Some(mut entry) = self.retirement.get_mut().take() else {
            return;
        };
        if entry.value().wait_fence == 0 {
            return;
        }
        let Some(device) = self.device.upgrade() else {
            return;
        };
        entry.value_mut().wait_fence = entry
            .value()
            .wait_fence
            .max(self.buffer.lifecycle.lock().fence);
        entry.value_mut().buffer = Some(self.buffer.clone());
        device
            .state
            .lock()
            .graphics_detachments
            .commit_vacant(entry);
        let mut completion = device.completion.lock();
        super::super::device::advance_graphics_cleanup(&device, &mut completion);
    }
}
