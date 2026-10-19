//! Canonical inode 的唯一 Linux i_writecount；路径别名、OFD 与 exec 共用 actual inode owner。
use super::{FileSystemError, Inode};
use alloc::sync::Arc;
use spin::Mutex;

/// 正数是写访问租约，负数是执行 deny-write 租约；两种准入在同一短锁下互斥。
/// 不分配、不回调；只 pin actual inode 的 lease 在锁外执行最后 resource Drop。
#[derive(Debug)]
pub(crate) struct InodeWriteAccess(Mutex<i64>);
impl InodeWriteAccess {
    pub(crate) const fn new() -> Self {
        Self(Mutex::new(0))
    }
    fn acquire(&self, mode: AccessMode) -> Result<(), FileSystemError> {
        let mut count = self.0.lock();
        match mode {
            AccessMode::Write if *count < 0 => return Err(FileSystemError::TextBusy),
            AccessMode::Execute if *count > 0 => return Err(FileSystemError::TextBusy),
            AccessMode::Write => *count = count.checked_add(1).expect("inode writer overflow"),
            AccessMode::Execute => *count = count.checked_sub(1).expect("inode execution overflow"),
        }
        Ok(())
    }
    fn release(&self, mode: AccessMode) {
        let mut count = self.0.lock();
        match mode {
            AccessMode::Write => {
                assert!(*count > 0, "inode write lease released twice");
                *count -= 1;
            }
            AccessMode::Execute => {
                assert!(*count < 0, "inode execute lease released twice");
                *count += 1;
            }
        }
    }
}
#[derive(Clone, Copy)]
enum AccessMode {
    Write,
    Execute,
}

/// 无分配的 actual inode access；与 mount/打开路径租约分别拥有不同事实。
pub(crate) struct InodeAccessLease {
    inode: Arc<dyn Inode>,
    mode: AccessMode,
}
impl InodeAccessLease {
    pub(crate) fn write(inode: Arc<dyn Inode>) -> Result<Self, FileSystemError> {
        Self::acquire(inode, AccessMode::Write)
    }
    pub(crate) fn execute(inode: Arc<dyn Inode>) -> Result<Self, FileSystemError> {
        Self::acquire(inode, AccessMode::Execute)
    }
    fn acquire(inode: Arc<dyn Inode>, mode: AccessMode) -> Result<Self, FileSystemError> {
        if let Some(access) = inode.write_access() {
            access.acquire(mode)?;
        }
        Ok(Self { inode, mode })
    }
}
impl Drop for InodeAccessLease {
    fn drop(&mut self) {
        if let Some(access) = self.inode.write_access() {
            access.release(self.mode);
        }
    }
}
