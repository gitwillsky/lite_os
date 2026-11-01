//! VMO 及其全局编号：VMA、页缓存、共享内存与设备缓冲的共同后备。

use alloc::sync::Arc;

use native_runtime::Vmo;
use native_runtime::native_abi::vmo;

use super::{MemoryError, PAGE_SIZE};

/// 一个可映射的 VMO。
#[derive(Debug)]
pub(crate) struct MemoryObject {
    vmo: Vmo,
    koid: u64,
}

impl MemoryObject {
    /// 接管一个 VMO。
    ///
    /// # Errors
    ///
    /// 读取对象编号失败（句柄无效）。
    pub(crate) fn new(vmo: Vmo) -> Result<Arc<Self>, MemoryError> {
        let koid = vmo.basic().map_err(MemoryError::from_status)?.koid;
        Arc::try_new(Self { vmo, koid }).map_err(|_| MemoryError::OutOfMemory)
    }

    /// 创建 `size` 字节（按页取整）的零内容匿名对象；`resizable` 时允许之后调整大小。
    ///
    /// # Errors
    ///
    /// 内存不足。
    pub(crate) fn anonymous(size: usize, resizable: bool) -> Result<Arc<Self>, MemoryError> {
        let size = size
            .checked_next_multiple_of(PAGE_SIZE)
            .ok_or(MemoryError::OutOfMemory)?;
        let options = if resizable { vmo::RESIZABLE } else { 0 };
        Self::new(Vmo::create(size as u64, options).map_err(MemoryError::from_status)?)
    }

    /// VMO。
    pub(crate) fn vmo(&self) -> &Vmo {
        &self.vmo
    }

    /// 全局编号（共享 futex key、`/proc/<pid>/maps` 的 inode 列）。
    pub(crate) fn koid(&self) -> u64 {
        self.koid
    }

    /// 字节大小。
    ///
    /// # Errors
    ///
    /// 句柄无效。
    pub(crate) fn size(&self) -> Result<u64, MemoryError> {
        self.vmo.size().map_err(MemoryError::from_status)
    }

    /// 本对象自己持有的已提交字节（驻留页统计）。
    ///
    /// # Errors
    ///
    /// 句柄无效。
    pub(crate) fn committed_bytes(&self) -> Result<u64, MemoryError> {
        self.vmo
            .info()
            .map(|info| info.committed_bytes)
            .map_err(MemoryError::from_status)
    }

    /// 调整大小（对象须以可调整大小创建）。
    ///
    /// # Errors
    ///
    /// 对象不可调整大小或内存不足。
    pub(crate) fn set_size(&self, size: u64) -> Result<(), MemoryError> {
        self.vmo.set_size(size).map_err(MemoryError::from_status)
    }

    /// 写时复制快照：父子此后互不可见对方的写入（fork 的私有映射）。
    ///
    /// # Errors
    ///
    /// 内存不足。
    pub(crate) fn snapshot(&self, offset: u64, size: u64) -> Result<Arc<Self>, MemoryError> {
        Self::new(
            self.vmo
                .create_child(vmo::CHILD_SNAPSHOT, offset, size)
                .map_err(MemoryError::from_status)?,
        )
    }

    /// 至少写时复制的子对象：未写入页跟随父对象的当前内容（文件的私有映射）。
    ///
    /// # Errors
    ///
    /// 内存不足。
    pub(crate) fn private_view(&self, offset: u64, size: u64) -> Result<Arc<Self>, MemoryError> {
        Self::new(
            self.vmo
                .create_child(vmo::CHILD_SNAPSHOT_AT_LEAST_ON_WRITE, offset, size)
                .map_err(MemoryError::from_status)?,
        )
    }
}
