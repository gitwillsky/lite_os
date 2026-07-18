use alloc::sync::Arc;

use super::{
    DrmError, DrmFile, DumbBuffer, DumbBufferInfo, Framebuffer,
    publication_order::{after_copyout, rollback_latest_u32, rollback_latest_u64},
};
use crate::fallible_tree::VacantEntry;

pub(super) struct DumbHandleReservation<'file> {
    file: &'file DrmFile,
    pub(super) handle: u32,
}

impl<'file> DumbHandleReservation<'file> {
    pub(super) fn reserve(file: &'file DrmFile) -> Result<Self, DrmError> {
        let handle = {
            let mut state = file.state.lock();
            let handle = state.next_handle;
            state.next_handle = handle.checked_add(1).ok_or(DrmError::NoSpace)?;
            handle
        };
        Ok(Self { file, handle })
    }

    fn commit(self) {
        // namespace publication 已接管 identity；抑制 rollback-only Drop，不遗留 owned resource。
        core::mem::forget(self);
    }
}

impl Drop for DumbHandleReservation<'_> {
    fn drop(&mut self) {
        let mut state = self.file.state.lock();
        rollback_latest_u32(&mut state.next_handle, self.handle);
    }
}

pub(super) struct BufferIdentityReservation<'file> {
    file: &'file DrmFile,
    pub(super) identity: u64,
}

impl<'file> BufferIdentityReservation<'file> {
    pub(super) fn reserve(file: &'file DrmFile) -> Result<Self, DrmError> {
        let identity = {
            let mut state = file.device.state.lock();
            let identity = state.next_buffer_identity;
            state.next_buffer_identity = identity.checked_add(1).ok_or(DrmError::NoSpace)?;
            identity
        };
        Ok(Self { file, identity })
    }

    fn commit(self) {
        // namespace publication 已接管 identity；抑制 rollback-only Drop，不遗留 owned resource。
        core::mem::forget(self);
    }
}

impl Drop for BufferIdentityReservation<'_> {
    fn drop(&mut self) {
        let mut state = self.file.device.state.lock();
        rollback_latest_u64(&mut state.next_buffer_identity, self.identity);
    }
}

/// @description 已完成 backing/node/handle 预留、尚未发布到 file namespace 的 dumb buffer。
pub(crate) struct PreparedDumbBuffer<'file> {
    handle: DumbHandleReservation<'file>,
    identity: BufferIdentityReservation<'file>,
    entry: VacantEntry<u32, Arc<DumbBuffer>>,
    info: DumbBufferInfo,
}

impl<'file> PreparedDumbBuffer<'file> {
    pub(super) fn new(
        handle: DumbHandleReservation<'file>,
        identity: BufferIdentityReservation<'file>,
        entry: VacantEntry<u32, Arc<DumbBuffer>>,
        info: DumbBufferInfo,
    ) -> Self {
        Self {
            handle,
            identity,
            entry,
            info,
        }
    }

    /// @description 执行完整 UAPI copyout，并只在成功后无分配发布 handle。
    /// @param copyout 接收无 pointer 结果并完成 userspace 输出。
    /// @return copyout 与 publication 全部成功。
    /// @errors 原样转发 copyout 错误；错误路径回收 backing/node 并释放未发布 handle。
    pub(crate) fn complete<E>(
        self,
        copyout: impl FnOnce(DumbBufferInfo) -> Result<(), E>,
    ) -> Result<(), E> {
        after_copyout(
            self,
            |prepared| copyout(prepared.info),
            PreparedDumbBuffer::publish,
        )
    }

    fn publish(self) {
        let Self {
            handle,
            identity,
            entry,
            info: _,
        } = self;
        handle.file.state.lock().buffers.commit_vacant(entry);
        handle.commit();
        identity.commit();
    }
}

pub(super) struct FramebufferIdReservation<'file> {
    file: &'file DrmFile,
    pub(super) id: u32,
}

impl<'file> FramebufferIdReservation<'file> {
    pub(super) fn reserve(file: &'file DrmFile) -> Result<Self, DrmError> {
        let id = {
            let mut state = file.device.state.lock();
            let id = state.next_framebuffer_id;
            state.next_framebuffer_id = id.checked_add(1).ok_or(DrmError::NoSpace)?;
            id
        };
        Ok(Self { file, id })
    }

    fn commit(self) {
        // namespace publication 已接管 identity；抑制 rollback-only Drop，不遗留 owned resource。
        core::mem::forget(self);
    }
}

impl Drop for FramebufferIdReservation<'_> {
    fn drop(&mut self) {
        let mut state = self.file.device.state.lock();
        rollback_latest_u32(&mut state.next_framebuffer_id, self.id);
    }
}

/// @description 已完成 KMS object/node 预留、尚未发布到 device namespace 的 framebuffer。
pub(crate) struct PreparedFramebuffer<'file> {
    id: FramebufferIdReservation<'file>,
    entry: VacantEntry<u32, Framebuffer>,
}

impl<'file> PreparedFramebuffer<'file> {
    pub(super) fn new(
        id: FramebufferIdReservation<'file>,
        entry: VacantEntry<u32, Framebuffer>,
    ) -> Self {
        Self { id, entry }
    }

    /// @description 执行完整 UAPI copyout，并只在成功后无分配发布 framebuffer ID。
    /// @param copyout 接收预留 ID 并完成 userspace 输出。
    /// @return copyout 与 publication 全部成功。
    /// @errors 原样转发 copyout 错误；错误路径回收 node/object 并释放未发布 ID。
    pub(crate) fn complete<E>(self, copyout: impl FnOnce(u32) -> Result<(), E>) -> Result<(), E> {
        after_copyout(
            self,
            |prepared| copyout(prepared.id.id),
            PreparedFramebuffer::publish,
        )
    }

    fn publish(self) {
        let Self { id, entry } = self;
        id.file
            .device
            .state
            .lock()
            .framebuffers
            .commit_vacant(entry);
        id.commit();
    }
}
