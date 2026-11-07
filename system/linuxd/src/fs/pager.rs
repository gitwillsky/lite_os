//! linuxd 的文件内容 pager：tmpfs/memfd 与 Linux FUSE 守护进程文件经进程内共用的 [`pager_pool`] 补页。
//!
//! PageSource 拥有补页政策：持久文件从存储读入，内存文件在实际页提交时检查配额。
//! 内核只请求/接收页；文件长度、存储限额和失败语义均由 linuxd 的对应 owner 决定。

use alloc::sync::Arc;

use native_runtime::Pager;
pub(super) use pager_pool::PageSource;

use crate::memory::MemoryObject;

/// 为文件内容创建可增长 pager VMO，返回对象与唯一请求 key。
///
/// # Errors
///
/// VMO 创建失败。
pub(super) fn create_object(
    size: u64,
    options: u32,
) -> Result<(Arc<MemoryObject>, u64), crate::fs::FileSystemError> {
    let (vmo, key) = pager_pool::create_vmo(size, options)
        .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
    let object = MemoryObject::new(vmo).map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
    Ok((object, key))
}

/// 发布 key 对应的内容 owner；只能在对象完成构造后调用。
/// 注册失败返回 OutOfMemory，尚未发表的对象仍由调用者回滚。
pub(super) fn register<T: PageSource + 'static>(
    key: u64,
    file: &Arc<T>,
) -> Result<(), crate::fs::FileSystemError> {
    let source: Arc<dyn PageSource> = file.clone();
    pager_pool::register(key, Arc::downgrade(&source))
        .map_err(|_| crate::fs::FileSystemError::OutOfMemory)
}

/// 同步写入与后台补页共用的 native pager。
pub(super) fn handle() -> &'static Pager {
    pager_pool::handle()
}

/// 内容 owner 退休：分离 pager 与对象后注销 key（见 [`pager_pool::retire`]）。
pub(super) fn retire(key: u64, object: &MemoryObject) {
    pager_pool::retire(key, object.vmo());
}

/// 回写开始（见 [`pager_pool::begin_writeback`]）。
pub(super) fn begin_writeback(object: &MemoryObject, offset: u64, length: u64) {
    pager_pool::begin_writeback(object.vmo(), offset, length);
}

/// 回写完成：存储已确认区间内容（见 [`pager_pool::end_writeback`]）。
pub(super) fn end_writeback(object: &MemoryObject, offset: u64, length: u64) {
    pager_pool::end_writeback(object.vmo(), offset, length);
}

/// 区间内容已与存储一致（见 [`pager_pool::mark_clean`]）。
pub(super) fn mark_clean(object: &MemoryObject, offset: u64, length: u64) {
    pager_pool::mark_clean(object.vmo(), offset, length);
}

/// `[offset, offset + length)` 中第一段脏区间。
pub(super) fn first_dirty(object: &MemoryObject, offset: u64, length: u64) -> Option<(u64, u64)> {
    pager_pool::first_dirty(object.vmo(), offset, length)
}
