use alloc::sync::Arc;

use super::{
    DrmError, DrmFile, DumbBuffer, DumbBufferInfo, Framebuffer,
    publication_order::{ReservationError, UnpublishedId, after_copyout},
};
use crate::fallible_tree::VacantEntry;

fn reservation_error(error: ReservationError) -> DrmError {
    match error {
        ReservationError::OutOfMemory => DrmError::OutOfMemory,
        ReservationError::NoSpace => DrmError::NoSpace,
    }
}

pub(super) struct DumbHandleReservation<'file> {
    file: &'file DrmFile,
    pub(super) handle: u32,
    reservation: Option<UnpublishedId<u32>>,
}

impl<'file> DumbHandleReservation<'file> {
    pub(super) fn reserve(file: &'file DrmFile) -> Result<Self, DrmError> {
        let reservation = {
            let mut state = file.state.lock();
            state.handle_ids.reserve().map_err(reservation_error)?
        };
        Ok(Self {
            file,
            handle: reservation.id(),
            reservation: Some(reservation),
        })
    }

    fn commit(mut self) {
        let reservation = self
            .reservation
            .take()
            .expect("published DRM handle lost reservation");
        self.file.state.lock().handle_ids.publish(reservation);
    }
}

impl Drop for DumbHandleReservation<'_> {
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            self.file.state.lock().handle_ids.rollback(reservation);
        }
    }
}

pub(super) struct BufferIdentityReservation<'file> {
    file: &'file DrmFile,
    pub(super) identity: u64,
    reservation: Option<UnpublishedId<u64>>,
}

impl<'file> BufferIdentityReservation<'file> {
    pub(super) fn reserve(file: &'file DrmFile) -> Result<Self, DrmError> {
        let reservation = {
            let mut state = file.device.state.lock();
            state
                .buffer_identities
                .reserve()
                .map_err(reservation_error)?
        };
        Ok(Self {
            file,
            identity: reservation.id(),
            reservation: Some(reservation),
        })
    }

    fn commit(mut self) {
        let reservation = self
            .reservation
            .take()
            .expect("published DRM buffer identity lost reservation");
        self.file
            .device
            .state
            .lock()
            .buffer_identities
            .publish(reservation);
    }
}

impl Drop for BufferIdentityReservation<'_> {
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            self.file
                .device
                .state
                .lock()
                .buffer_identities
                .rollback(reservation);
        }
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
    reservation: Option<UnpublishedId<u32>>,
}

impl<'file> FramebufferIdReservation<'file> {
    pub(super) fn reserve(file: &'file DrmFile) -> Result<Self, DrmError> {
        let reservation = {
            let mut state = file.device.state.lock();
            state.framebuffer_ids.reserve().map_err(reservation_error)?
        };
        Ok(Self {
            file,
            id: reservation.id(),
            reservation: Some(reservation),
        })
    }

    fn commit(mut self) {
        let reservation = self
            .reservation
            .take()
            .expect("published DRM framebuffer ID lost reservation");
        self.file
            .device
            .state
            .lock()
            .framebuffer_ids
            .publish(reservation);
    }
}

impl Drop for FramebufferIdReservation<'_> {
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            self.file
                .device
                .state
                .lock()
                .framebuffer_ids
                .rollback(reservation);
        }
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
