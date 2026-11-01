use super::*;
use spin::Once;

// OWNER: fs page-cache owns every regular inode cached page until clean reclaim. A weak-only map
// would lose dirty MAP_SHARED pages when the final VMA disappears before sync(2).
static FILES: Once<Mutex<FallibleMap<SharedFileId, Arc<CachedFile>>>> = Once::new();

pub(super) fn cached_file(
    inode: Arc<dyn Inode>,
) -> Result<Option<Arc<CachedFile>>, FileSystemError> {
    if !matches!(inode.inode_type(), InodeType::File | InodeType::BlockDevice) {
        return Err(FileSystemError::InvalidOperation);
    }
    let id = inode.page_cache_id()?;
    let registry = FILES.call_once(|| Mutex::new(FallibleMap::new()));
    {
        let files = registry.lock();
        // FILES → claim 的短查询与 cache publication 同锁。claim 领取后锁外逐出 FILES；
        // 因此 reader 要么先被逐出/判 Busy，要么看见新 claim 并直接读块层。
        if !matches!(inode.data_backing(), DataBacking::PageCache) {
            return Ok(None);
        }
        if let Some(file) = files.get(&id) {
            return Ok(Some(file.clone()));
        }
    }
    // allocation/registration 可重入 direct reclaim，不能保留 FILES spin guard。
    let file_slot = FallibleMap::<SharedFileId, Arc<CachedFile>>::try_reserve_node()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let writer_retired =
        Arc::try_new(TaskEvent::new()).map_err(|_| FileSystemError::OutOfMemory)?;
    let file = Arc::try_new(CachedFile {
        id,
        inode,
        operation: TaskMutex::new(()),
        write_sequence: TaskMutex::new(()),
        pages: Mutex::new(CachedPages::new()),
        storage_epoch: StorageEpoch::new(),
        writer_retired,
    })
    .map_err(|_| FileSystemError::OutOfMemory)?;
    // registry 只持 Weak；未发布 candidate 是空 cache，可安全回收。竞争败者在锁外
    // Drop，weak slot 变成下一次注册可复用的 tombstone，不会有第二个强 owner。
    register_memory_reclaimer(file.clone()).map_err(shared_error)?;
    let admitted = {
        let mut files = registry.lock();
        if !matches!(file.inode.data_backing(), DataBacking::PageCache) {
            None
        } else if let Some(existing) = files.get(&id) {
            Some(existing.clone())
        } else {
            files.commit_vacant(file_slot.fill(id, file.clone()));
            Some(file.clone())
        }
    };
    Ok(admitted)
}

/// 写回并逐出一个 filesystem instance 的全部 cached file（umount 的 `sync_filesystem` +
/// `evict_inodes`）。
///
/// 写回失败不阻止逐出：卸载已提交，与 Linux 相同只报告错误。
///
/// # Errors
///
/// 返回第一个写回错误。
pub(crate) fn evict_filesystem(filesystem_id: usize) -> Result<(), FileSystemError> {
    let mut cursor = None;
    let mut result = Ok(());
    while let Some((id, file)) = next_cached(cursor) {
        cursor = Some(id);
        if id.filesystem != filesystem_id {
            continue;
        }
        let written = writeback_file(&file);
        if let Err(error) = written {
            result = result.and(Err(error));
        }
        let removed = {
            let mut files = FILES.wait().lock();
            if files
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(current, &file))
            {
                files.remove(&id)
            } else {
                None
            }
        };
        // 旧 inode 的 final Drop 可重新进入 filesystem；禁止在 FILES lock 内析构。
        drop(removed);
    }
    result
}

/// 逐出一个已失去全部 link 的 inode 的 page cache（Linux `evict_inode` 对 `i_nlink == 0` 的路径）。
///
/// 全局 registry 持有 `CachedFile`，而 `CachedFile` 强持有 inode；没有这一步，已删除文件的 inode
/// 与内容会一直活到下一次 `sync`，ext4 的 orphan 回收（释放数据块）也被推迟到那时。已删除文件的脏页
/// 没有任何持久化意义，直接丢弃。
///
/// # Parameters
///
/// - `inode`: 刚被删除链接或刚失去最后一个打开条目的 inode。
///
/// 仅由最后 Dentry Drop 调用；其他 pathname/mmap 必须先归还实际 inode pin。
/// 临时 I/O 的 CachedFile Arc 可继续保活，但不能阻止 registry 退出；最后 I/O 结束自然回收。
/// cache 与最后 dentry 各持一个 inode 引用，不接受 caller 自报引用数。
///
/// # Returns
///
/// 无；非 page-cache 文件、仍有 link、仍被引用或没有 cache 时为空操作。
pub(in crate::fs) fn evict_if_unlinked(inode: &Arc<dyn Inode>) {
    if inode.inode_type() != InodeType::File
        || !matches!(inode.data_backing(), DataBacking::PageCache)
    {
        return;
    }
    let Some((number, links)) = inode.notification_link_state() else {
        return;
    };
    if links != 0 {
        return;
    }
    let id = SharedFileId {
        filesystem: inode.filesystem_id(),
        inode: number,
    };
    let evicted = {
        let mut files = FILES.call_once(|| Mutex::new(FallibleMap::new())).lock();
        let removable = files
            .get(&id)
            .is_some_and(|file| Arc::strong_count(&file.inode) == 2);
        if removable { files.remove(&id) } else { None }
    };
    // 最后一个引用在 registry 锁之外释放：页释放与 inode 的最终回收都可能进入 filesystem。
    drop(evicted);
}

fn lookup_cached(id: SharedFileId) -> Option<Arc<CachedFile>> {
    FILES
        .call_once(|| Mutex::new(FallibleMap::new()))
        .lock()
        .get(&id)
        .cloned()
}

/// 写回 `id` 的全部脏页；没有缓存时为空操作。
///
/// # Errors
///
/// 写回失败透传 filesystem error。
pub(crate) fn writeback_cached(id: SharedFileId) -> Result<(), FileSystemError> {
    let Some(file) = lookup_cached(id) else {
        return Ok(());
    };
    writeback_file(&file)
}

/// 让一个文件的缓存内容失效（Linux `invalidate_inode_pages2` 的未映射部分），之后的读取重新向
/// filesystem 取数据。远端可被他人修改的 filesystem（FUSE）在打开文件或收到失效通知时调用。
///
/// 1. 与 write/truncate 同一 operation domain，并推进存储版本使在途的异步填页作废；
/// 2. 先写回脏页，再丢弃只被缓存持有的干净页；仍被映射或正在 I/O 的页保留到映射解除，
///    否则 mmap 进程手中的页与缓存分裂成两份内容。
///
/// # Parameters
///
/// - `wait`: true 时等待 write/truncate 结束（打开文件、守护进程通知：调用者不持该文件的页缓存锁）；
///   false 时只在锁空闲时作废（属性刷新与读入口：调用者可能正持同一文件的写序列，阻塞会自锁）。
///
/// # Errors
///
/// `wait` 为 false 且锁被占用返回 `Busy`，调用者保留作废需求稍后重试；写回失败透传，此时不丢弃
/// 任何页；锁元数据分配失败返回 `OutOfMemory`。没有缓存时为空操作。
pub(crate) fn invalidate(id: SharedFileId, wait: bool) -> Result<(), FileSystemError> {
    let Some(file) = lookup_cached(id) else {
        return Ok(());
    };
    let (_sequence, _operation) = if wait {
        let sequence = file
            .write_sequence
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let operation = file
            .operation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        (sequence, operation)
    } else {
        let sequence = file
            .write_sequence
            .try_lock()
            .ok_or(FileSystemError::Busy)?;
        let operation = file.operation.try_lock().ok_or(FileSystemError::Busy)?;
        (sequence, operation)
    };
    let _epoch = file.storage_epoch.begin_mutation();
    file.writeback_range(0, u64::MAX, false)?;
    file.pages
        .lock()
        .entries
        .retain(|_, page| !(page.reclaimable() && Arc::strong_count(page) == 1));
    Ok(())
}

/// 写回并逐出一个缓存文件（块设备在被文件系统挂载前使用）。
///
/// # Errors
///
/// 缓存仍被映射或有进行中的 I/O 返回 `Busy`；写回失败透传 filesystem error。没有缓存时为空操作。
pub(crate) fn evict_cached(id: SharedFileId) -> Result<(), FileSystemError> {
    writeback_cached(id)?;
    let Some(file) = lookup_cached(id) else {
        return Ok(());
    };
    let mut files = FILES.wait().lock();
    // registry 与本函数各持一个引用；更多说明有映射或进行中的 I/O，此时逐出会让它们读到过期数据。
    if Arc::strong_count(&file) > 2 {
        return Err(FileSystemError::Busy);
    }
    let removed = files.remove(&id);
    drop(files);
    drop(removed);
    Ok(())
}

pub(crate) fn sync_all() -> Result<(), FileSystemError> {
    let mut cursor = None;
    // 每次只 pin 一个 owner；不在 FILES lock 内为全局快照分配，不跨锁执行 writeback。
    // 逐 entry 取得稳定 owner；并发新增/进行中的 mutation 不承诺本轮持久化。
    while let Some((id, file)) = next_cached(cursor) {
        cursor = Some(id);
        writeback_file(&file)?;
        let removed = {
            let mut files = FILES.wait().lock();
            if files
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(current, &file))
                && Arc::strong_count(&file) == 2
                && file.pages.lock().entries.is_empty()
            {
                files.remove(&id)
            } else {
                None
            }
        };
        drop(removed);
    }
    Ok(())
}

fn next_cached(after: Option<SharedFileId>) -> Option<(SharedFileId, Arc<CachedFile>)> {
    let files = FILES.call_once(|| Mutex::new(FallibleMap::new())).lock();
    let next = match after {
        Some(id) => files.successor(&id),
        None => files.first_key_value(),
    };
    next.map(|(&id, file)| (id, file.clone()))
}

fn writeback_file(file: &CachedFile) -> Result<(), FileSystemError> {
    let _sequence = file
        .write_sequence
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let _operation = file
        .operation
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    file.writeback_range(0, u64::MAX, true)
}

/// 把 `id` 的脏页交给 filesystem 而不建立持久化边界（Linux `filemap_write_and_wait`）：FUSE 在
/// `close(2)` 发 FLUSH 前、以及最后一个可写句柄释放前调用，让缓冲写与 mmap 脏页用仍有效的句柄写出。
///
/// # Errors
///
/// 写回失败透传，页保持 dirty；锁元数据分配失败返回 `OutOfMemory`。没有缓存时为空操作。
pub(crate) fn write_back(id: SharedFileId) -> Result<(), FileSystemError> {
    let Some(file) = lookup_cached(id) else {
        return Ok(());
    };
    let _sequence = file
        .write_sequence
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let _operation = file
        .operation
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    file.writeback_range(0, u64::MAX, false)
}

/// 从唯一 CachedFile page maps 汇总一次全局 resident/dirty/reclaimable 快照。
///
/// # Returns
///
/// 只读统计；不触发 fill、writeback 或 reclaim。
pub(crate) fn statistics() -> PageCacheStatistics {
    let files = FILES.call_once(|| Mutex::new(FallibleMap::new())).lock();
    let mut statistics = PageCacheStatistics {
        resident_pages: 0,
        dirty_pages: 0,
        reclaimable_pages: 0,
    };
    for file in files.values() {
        let pages = file.pages.lock();
        statistics.resident_pages += pages.entries.len();
        for page in pages.entries.values() {
            statistics.dirty_pages += usize::from(page.dirty());
            statistics.reclaimable_pages +=
                usize::from(page.reclaimable() && Arc::strong_count(page) == 1);
        }
    }
    statistics
}
