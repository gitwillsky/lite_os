//! 标准 DRM PRIME 与 dma-buf 文件。资源/隐式 fence 属于 GEM，不保活导出 OFD 或它的 context。
use super::{DrmDevice, DrmError, DrmFile, DrmWait, graphics::VirglBuffer};
use crate::{
    fs::chardev::{DeviceError, DeviceFile, DeviceWaitSources, IoctlCall, MapRequest},
    memory::DeviceMappingSource,
};
use alloc::sync::Arc;
use syscall_abi::errno;

pub(super) struct DmaBuf {
    pub(super) device: Arc<DrmDevice>,
    buffer: Option<Arc<VirglBuffer>>,
    // OWNER: 只弱引用真实共享 OFD，重复 HANDLE_TO_FD 是 dup；强引用会形成 OFD -> DmaBuf -> OFD 环。
    description: spin::Mutex<Option<alloc::sync::Weak<dyn core::any::Any + Send + Sync>>>,
}
impl DrmFile {
    pub(super) fn export_prime(&self, handle: u32) -> Result<Arc<DmaBuf>, DrmError> {
        let buffer = self
            .state
            .lock()
            .graphics_buffers
            .get(&handle)
            .map(|entry| entry.buffer.clone())
            .ok_or(DrmError::NotFound)?;
        DmaBuf::from_buffer(&self.device, buffer)
    }
}
impl core::fmt::Debug for DmaBuf {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("DmaBuf")
            .field("resource_id", &self.buffer().resource_id)
            .finish_non_exhaustive()
    }
}
impl crate::memory::MappedFileOwner for DmaBuf {}
impl DmaBuf {
    // GEM VMA 与 PRIME FD 共用资源 lease；最后释放者主动推进 UNREF，不依赖后续无关 GPU 活动。
    pub(super) fn from_buffer(
        device: &Arc<DrmDevice>,
        buffer: Arc<VirglBuffer>,
    ) -> Result<Arc<Self>, DrmError> {
        if let Some(file) = buffer.lifecycle.lock().exported.upgrade() {
            return Ok(file);
        }
        let candidate = Arc::try_new(Self {
            device: device.clone(),
            buffer: Some(buffer.clone()),
            description: spin::Mutex::new(None),
        })
        .map_err(|_| DrmError::OutOfMemory)?;
        let mut lifecycle = buffer.lifecycle.lock();
        if let Some(file) = lifecycle.exported.upgrade() {
            // candidate Drop 会取得 completion 并检查资源 fence，不能持有 lifecycle 锁析构它。
            drop(lifecycle);
            return Ok(file);
        }
        lifecycle.exported = Arc::downgrade(&candidate);
        lifecycle.shared = true;
        drop(lifecycle);
        Ok(candidate)
    }
    pub(super) fn buffer(&self) -> &Arc<VirglBuffer> {
        self.buffer.as_ref().expect("live dma-buf lost GEM")
    }
}
impl DeviceFile for DmaBuf {
    fn shared_description(
        &self,
    ) -> Option<&spin::Mutex<Option<alloc::sync::Weak<dyn core::any::Any + Send + Sync>>>> {
        Some(&self.description)
    }
    fn supports_read(&self) -> bool {
        false
    }
    fn supports_write(&self) -> bool {
        false
    }
    fn anonymous_name(&self) -> &'static [u8] {
        b"/dmabuf"
    }
    fn seek(&self, offset: i64, whence: u32) -> Result<u64, DeviceError> {
        match (offset, whence) {
            (0, 0) => Ok(0),
            (0, 2) => Ok(self.buffer().size as u64),
            _ => Err(DeviceError::Errno(errno::EINVAL)),
        }
    }
    fn poll(&self, events: i16) -> i16 {
        let fence = self.buffer().lifecycle.lock().fence;
        if self.device.completion.lock().timeline.completed() >= fence {
            events & 0x005
        } else {
            0
        }
    }
    fn wait_sources(&self, _events: i16) -> DeviceWaitSources {
        DeviceWaitSources::pipe(self.device.completion_read.pipe(), 0x001)
    }
    fn readiness_generation(&self) -> u64 {
        let fence = self.buffer().lifecycle.lock().fence;
        if self.device.completion.lock().timeline.completed() >= fence {
            fence
        } else {
            0
        }
    }
    fn mmap(
        &self,
        offset: u64,
        length: usize,
        request: MapRequest,
    ) -> Result<DeviceMappingSource, DeviceError> {
        let end = usize::try_from(offset)
            .ok()
            .and_then(|offset| offset.checked_add(length));
        if !request.shared
            || request.executable
            || offset != 0
            || end.is_none_or(|end| end > self.buffer().size)
        {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        if request.writable && !request.fd_writable {
            return Err(DeviceError::Errno(errno::EACCES));
        }
        Ok(DeviceMappingSource::new(
            (1u64 << 63) | u64::from(self.buffer().resource_id),
            self.buffer().backing.clone(),
        ))
    }
    fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        // DMA_BUF_IOCTL_SYNC, Linux _IOW('b', 0, __u64)。一致 DMA 平台不增加 cache shadow。
        if call.request != 0x40086200 {
            return Err(DeviceError::Errno(errno::ENOTTY));
        }
        let mut bytes = [0; 8];
        call.user
            .read(call.argument, &mut bytes)
            .map_err(|_| DeviceError::Errno(errno::EFAULT))?;
        let flags = u64::from_ne_bytes(bytes);
        if flags & !7 != 0 || flags & 3 == 0 {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        if flags & 4 == 0 {
            let _reservation = self.buffer().reservation.lock_killable().map_err(|error| {
                DeviceError::Errno(match error {
                    crate::execution::sync::TaskMutexLockError::OutOfMemory => errno::ENOMEM,
                    crate::execution::sync::TaskMutexLockError::Interrupted => errno::EINTR,
                })
            })?;
            let fence = self.buffer().lifecycle.lock().fence;
            super::ioctl::wait_scanout(DrmWait {
                device: self.device.clone(),
                fence,
            })
            .map_err(DeviceError::Errno)?;
        }
        Ok(0)
    }
}
impl Drop for DmaBuf {
    fn drop(&mut self) {
        drop(self.buffer.take());
        let mut completion = self.device.completion.lock();
        super::device::advance_graphics_cleanup(&self.device, &mut completion);
    }
}
