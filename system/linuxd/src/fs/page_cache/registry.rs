//! 缓存文件的全局注册表：同一 inode 只有一个缓存文件。
//!
//! 注册表持有强引用：脏的 `MAP_SHARED` 页在最后一个映射消失后仍须保留到写回。

use super::*;
use crate::sync::{Mutex, Once};
use fallible_tree::FallibleMap;

// OWNER: 全部缓存文件；查询与发布同锁，同一 inode 不会出现两个缓存文件。
static FILES: Once<Mutex<FallibleMap<SharedFileId, Arc<CachedFile>>>> = Once::new();

fn files() -> &'static Mutex<FallibleMap<SharedFileId, Arc<CachedFile>>> {
    FILES.call_once(|| Mutex::new(FallibleMap::new()))
}

impl Drop for CachedFile {
    fn drop(&mut self) {
        pager::unregister(self.key);
    }
}

pub(super) fn cached_file(
    inode: Arc<dyn Inode>,
) -> Result<Option<Arc<CachedFile>>, FileSystemError> {
    if !matches!(inode.inode_type(), InodeType::File | InodeType::BlockDevice) {
        return Err(FileSystemError::InvalidOperation);
    }
    if !matches!(inode.data_backing(), DataBacking::PageCache) {
        return Ok(None);
    }
    let id = inode.page_cache_id()?;
    if let Some(file) = files().lock().get(&id) {
        return Ok(Some(file.clone()));
    }
    let slot = FallibleMap::<SharedFileId, Arc<CachedFile>>::try_reserve_node()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let (object, key) = pager::create_object(inode.size())?;
    let file = Arc::try_new(CachedFile {
        id,
        inode,
        object,
        key,
        operation: TaskMutex::new(()),
        write_sequence: TaskMutex::new(()),
    })
    .map_err(|_| FileSystemError::OutOfMemory)?;
    let admitted = {
        let mut files = files().lock();
        match files.get(&id) {
            Some(existing) => existing.clone(),
            None => {
                files.commit_vacant(slot.fill(id, file.clone()));
                pager::register(key, &file);
                file.clone()
            }
        }
    };
    // 竞争失败的候选在锁外释放。
    drop(file);
    Ok(Some(admitted))
}

fn lookup_cached(id: SharedFileId) -> Option<Arc<CachedFile>> {
    files().lock().get(&id).cloned()
}

fn next_cached(after: Option<SharedFileId>) -> Option<(SharedFileId, Arc<CachedFile>)> {
    let files = files().lock();
    let next = match after {
        Some(id) => files.successor(&id),
        None => files.first_key_value(),
    };
    next.map(|(&id, file)| (id, file.clone()))
}

fn writeback_file(file: &CachedFile, durable: bool) -> Result<(), FileSystemError> {
    let _sequence = file.lock(&file.write_sequence)?;
    let _operation = file.lock(&file.operation)?;
    file.writeback_range(0, u64::MAX, durable)
}

/// 移除注册（只在没有其他持有者时），最后的引用在注册表锁外释放。
fn remove_if_unused(id: SharedFileId, file: &Arc<CachedFile>) {
    let removed = {
        let mut files = files().lock();
        if files
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(current, file))
            && Arc::strong_count(file) == 2
        {
            files.remove(&id)
        } else {
            None
        }
    };
    drop(removed);
}

/// 卸载文件系统：写回并移除其全部缓存文件。
pub(crate) fn evict_filesystem(filesystem_id: usize) -> Result<(), FileSystemError> {
    let mut cursor = None;
    let mut result = Ok(());
    while let Some((id, file)) = next_cached(cursor) {
        cursor = Some(id);
        if id.filesystem != filesystem_id {
            continue;
        }
        if let Err(error) = writeback_file(&file, false) {
            result = result.and(Err(error));
        }
        let removed = {
            let mut files = files().lock();
            if files
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(current, &file))
            {
                files.remove(&id)
            } else {
                None
            }
        };
        drop(removed);
    }
    result
}

/// 最后一个链接被删除的 inode 不再需要缓存。
pub(in crate::fs) fn evict_if_unlinked(inode: &Arc<dyn Inode>) {
    if inode.inode_type() != InodeType::File
        || !matches!(inode.data_backing(), DataBacking::PageCache)
    {
        return;
    }
    let Some((number, 0)) = inode.notification_link_state() else {
        return;
    };
    let id = SharedFileId {
        filesystem: inode.filesystem_id(),
        inode: number,
    };
    let evicted = {
        let mut files = files().lock();
        let removable = files
            .get(&id)
            .is_some_and(|file| Arc::strong_count(&file.inode) == 2);
        if removable { files.remove(&id) } else { None }
    };
    drop(evicted);
}

/// 写回一个缓存文件的脏页。
pub(crate) fn writeback_cached(id: SharedFileId) -> Result<(), FileSystemError> {
    match lookup_cached(id) {
        Some(file) => writeback_file(&file, false),
        None => Ok(()),
    }
}

/// 写回后丢弃缓存页（远端内容变化、`drop_caches`）；`wait` 为假时锁忙即返回 `Busy`。
pub(crate) fn invalidate(id: SharedFileId, wait: bool) -> Result<(), FileSystemError> {
    let Some(file) = lookup_cached(id) else {
        return Ok(());
    };
    let (_sequence, _operation) = if wait {
        (
            file.lock(&file.write_sequence)?,
            file.lock(&file.operation)?,
        )
    } else {
        (
            file.write_sequence
                .try_lock()
                .ok_or(FileSystemError::Busy)?,
            file.operation.try_lock().ok_or(FileSystemError::Busy)?,
        )
    };
    file.writeback_range(0, u64::MAX, false)?;
    file.drop_range(0, u64::MAX)
}

/// 写回并移除一个缓存文件；仍被映射或使用时返回 `Busy`。
pub(crate) fn evict_cached(id: SharedFileId) -> Result<(), FileSystemError> {
    writeback_cached(id)?;
    let Some(file) = lookup_cached(id) else {
        return Ok(());
    };
    if Arc::strong_count(&file) > 2 {
        return Err(FileSystemError::Busy);
    }
    remove_if_unused(id, &file);
    Ok(())
}

/// `sync(2)`：持久写回全部缓存文件，并移除不再使用的干净缓存。
pub(crate) fn sync_all() -> Result<(), FileSystemError> {
    let mut cursor = None;
    while let Some((id, file)) = next_cached(cursor) {
        cursor = Some(id);
        writeback_file(&file, true)?;
        remove_if_unused(id, &file);
    }
    Ok(())
}

/// 写回一个缓存文件（不要求持久化）。
pub(crate) fn write_back(id: SharedFileId) -> Result<(), FileSystemError> {
    writeback_cached(id)
}

/// 全局页统计。
pub(crate) fn statistics() -> PageCacheStatistics {
    let mut statistics = PageCacheStatistics {
        resident_pages: 0,
        dirty_pages: 0,
    };
    let mut cursor = None;
    while let Some((id, file)) = next_cached(cursor) {
        cursor = Some(id);
        statistics.resident_pages +=
            file.object.committed_bytes().unwrap_or(0) as usize / PAGE_SIZE;
        let mut offset = 0;
        while let Some((start, length)) =
            pager::first_dirty(&file.object, offset, u64::MAX - offset)
        {
            statistics.dirty_pages += (length / PAGE_SIZE as u64) as usize;
            offset = start + length;
        }
    }
    statistics
}
