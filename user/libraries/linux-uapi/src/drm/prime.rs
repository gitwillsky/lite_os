//! Linux DRM PRIME fd 与 file-local GEM handle；无私有 buffer identity 或跨进程指针。
use super::{DrmDevice, GemHandle};
use crate::raw;
use std::{
    io,
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
};
#[repr(C)]
#[derive(Default)]
struct PrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
}
impl DrmDevice {
    /// 导出已存在的 GEM handle。返回拥有的 O_CLOEXEC|O_RDWR dma-buf fd；无效句柄/设备错误返回 io::Error。
    pub fn export_dma_buf(&self, handle: GemHandle) -> io::Result<OwnedFd> {
        let mut request = PrimeHandle {
            handle: handle.get(),
            flags: raw::O_CLOEXEC as u32 | 2,
            fd: -1,
        };
        self.ioctl(0xc00c642d, (&raw mut request).cast())?;
        if request.fd < 0 {
            return Err(io::Error::other("PRIME returned an invalid fd"));
        }
        // SAFETY: 成功的 HANDLE_TO_FD 发布一个新的、由调用者唯一拥有的 fd，转交 OwnedFd 一次。
        Ok(unsafe { OwnedFd::from_raw_fd(request.fd) })
    }
    /// 导入 dma-buf，返回本 OFD 唯一对应的 GEM handle；重复导入不增加 kernel handle 引用计数。
    /// 调用者必须合并同号 handle 的用户态 owner，仅在最后一次使用后执行 GEM_CLOSE。
    pub fn import_dma_buf(&self, fd: BorrowedFd<'_>) -> io::Result<GemHandle> {
        let mut request = PrimeHandle {
            fd: fd.as_raw_fd(),
            ..Default::default()
        };
        self.ioctl(0xc00c642e, (&raw mut request).cast())?;
        GemHandle::new(request.handle)
            .ok_or_else(|| io::Error::other("PRIME returned GEM handle zero"))
    }
}
