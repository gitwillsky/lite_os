use alloc::{collections::BTreeMap, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use spin::{Mutex, Once};

use crate::memory::{
    MemoryReclaimer, PAGE_SIZE, SharedFileError, SharedFileId, SharedFileMapping, SharedFrame,
    SharedPage, invalidate_shared_file, register_memory_reclaimer,
};

use super::{FileSystemError, Inode, InodeType};

#[derive(Debug)]
struct CachedPage {
    frame: SharedFrame,
    // OWNER: CachedPage 独占 dirty 与 writable-PTE 引用计数；拆到 VMA 会使 fork/unmap
    // 无法判断 writeback 后是否仍可能被用户硬件写入，造成脏数据丢失。
    dirty: AtomicBool,
    writers: AtomicUsize,
}

impl SharedPage for CachedPage {
    fn frame(&self) -> &SharedFrame {
        &self.frame
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    fn acquire_writer(&self) {
        self.writers.fetch_add(1, Ordering::AcqRel);
        self.mark_dirty();
    }

    fn release_writer(&self) {
        let previous = self.writers.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous != 0);
    }
}

struct CachedFile {
    id: SharedFileId,
    inode: Arc<dyn Inode>,
    // OWNER: 单 inode operation lock 串行化 append、truncate 与 writeback 的 size 边界。
    operation: Mutex<()>,
    pages: Mutex<BTreeMap<u64, Arc<CachedPage>>>,
}

impl core::fmt::Debug for CachedFile {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("CachedFile")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl CachedFile {
    fn page(&self, index: u64) -> Result<Arc<CachedPage>, FileSystemError> {
        if let Some(page) = self.pages.lock().get(&index).cloned() {
            return Ok(page);
        }
        let frame = SharedFrame::allocate().map_err(shared_error)?;
        let offset = index
            .checked_mul(PAGE_SIZE as u64)
            .ok_or(FileSystemError::InvalidOperation)?;
        if offset >= self.inode.size() {
            return Err(FileSystemError::InvalidOperation);
        }
        let available = usize::try_from(self.inode.size() - offset)
            .unwrap_or(usize::MAX)
            .min(PAGE_SIZE);
        let mut contents = vec![0; available];
        let read = self.inode.read_storage(offset, &mut contents)?;
        if read != available {
            return Err(FileSystemError::IoError);
        }
        frame.write(0, &contents);
        let page = Arc::new(CachedPage {
            frame,
            dirty: AtomicBool::new(false),
            writers: AtomicUsize::new(0),
        });
        let mut pages = self.pages.lock();
        Ok(pages.entry(index).or_insert_with(|| page.clone()).clone())
    }

    fn update_cached(&self, offset: u64, input: &[u8]) -> Result<(), FileSystemError> {
        let mut done = 0;
        while done < input.len() {
            let current = offset + done as u64;
            let index = current / PAGE_SIZE as u64;
            let page_offset = current as usize % PAGE_SIZE;
            let count = (PAGE_SIZE - page_offset).min(input.len() - done);
            if let Some(page) = self.pages.lock().get(&index).cloned() {
                page.frame.write(page_offset, &input[done..done + count]);
            }
            done += count;
        }
        Ok(())
    }

    fn writeback_range(&self, offset: u64, length: u64) -> Result<(), FileSystemError> {
        let end = offset.saturating_add(length);
        let pages: Vec<_> = self
            .pages
            .lock()
            .range((offset / PAGE_SIZE as u64)..=((end.saturating_sub(1)) / PAGE_SIZE as u64))
            .map(|(index, page)| (*index, page.clone()))
            .collect();
        for (index, page) in pages {
            if !page.dirty.load(Ordering::Acquire) {
                continue;
            }
            let page_start = index * PAGE_SIZE as u64;
            let count = usize::try_from(self.inode.size().saturating_sub(page_start))
                .unwrap_or(usize::MAX)
                .min(PAGE_SIZE);
            if count == 0 {
                continue;
            }
            let mut data = vec![0; count];
            page.frame.read(0, &mut data);
            if self.inode.write_storage(page_start, &data)? != count {
                return Err(FileSystemError::IoError);
            }
            if page.writers.load(Ordering::Acquire) == 0 {
                page.dirty.store(false, Ordering::Release);
            }
        }
        self.inode.sync_storage()
    }
}

impl SharedFileMapping for CachedFile {
    fn id(&self) -> SharedFileId {
        self.id
    }

    fn size(&self) -> u64 {
        self.inode.size()
    }

    fn page(&self, index: u64) -> Result<Arc<dyn SharedPage>, SharedFileError> {
        CachedFile::page(self, index)
            .map(|page| page as Arc<dyn SharedPage>)
            .map_err(fs_error)
    }

    fn sync_range(&self, offset: u64, length: u64) -> Result<(), SharedFileError> {
        let _operation = self.operation.lock();
        self.writeback_range(offset, length).map_err(fs_error)
    }
}

impl MemoryReclaimer for CachedFile {
    fn reclaim_pages(&self, limit: usize) -> usize {
        let Some(mut pages) = self.pages.try_lock() else {
            return 0;
        };
        let mut reclaimed = 0;
        pages.retain(|_, page| {
            let reclaim = reclaimed < limit
                && !page.dirty.load(Ordering::Acquire)
                && page.writers.load(Ordering::Acquire) == 0
                && Arc::strong_count(page) == 1;
            reclaimed += usize::from(reclaim);
            !reclaim
        });
        reclaimed
    }
}

// OWNER: fs page-cache owns every regular inode cached page until clean reclaim. A weak-only map
// would lose dirty MAP_SHARED pages when the final VMA disappears before sync(2).
static FILES: Once<Mutex<BTreeMap<SharedFileId, Arc<CachedFile>>>> = Once::new();

fn cached_file(inode: Arc<dyn Inode>) -> Result<Arc<CachedFile>, FileSystemError> {
    if inode.inode_type() != InodeType::File {
        return Err(FileSystemError::InvalidOperation);
    }
    let id = SharedFileId {
        filesystem: inode.filesystem_id(),
        inode: inode.metadata()?.inode,
    };
    let mut files = FILES.call_once(|| Mutex::new(BTreeMap::new())).lock();
    if let Some(file) = files.get(&id) {
        return Ok(file.clone());
    }
    let file = Arc::new(CachedFile {
        id,
        inode,
        operation: Mutex::new(()),
        pages: Mutex::new(BTreeMap::new()),
    });
    register_memory_reclaimer(file.clone()).map_err(shared_error)?;
    files.insert(id, file.clone());
    Ok(file)
}

pub(crate) fn mapping(
    inode: Arc<dyn Inode>,
) -> Result<Arc<dyn SharedFileMapping>, FileSystemError> {
    cached_file(inode).map(|file| file as Arc<dyn SharedFileMapping>)
}

pub(crate) fn read(
    inode: Arc<dyn Inode>,
    offset: u64,
    output: &mut [u8],
) -> Result<usize, FileSystemError> {
    if inode.inode_type() != InodeType::File {
        return inode.read_storage(offset, output);
    }
    let file = cached_file(inode)?;
    let size = file.inode.size();
    let count = usize::try_from(size.saturating_sub(offset))
        .unwrap_or(usize::MAX)
        .min(output.len());
    let mut done = 0;
    while done < count {
        let current = offset + done as u64;
        let page = file.page(current / PAGE_SIZE as u64)?;
        let page_offset = current as usize % PAGE_SIZE;
        let part = (PAGE_SIZE - page_offset).min(count - done);
        page.frame.read(page_offset, &mut output[done..done + part]);
        done += part;
    }
    Ok(done)
}

pub(crate) fn write(
    inode: Arc<dyn Inode>,
    offset: u64,
    input: &[u8],
) -> Result<usize, FileSystemError> {
    if inode.inode_type() != InodeType::File {
        return inode.write_storage(offset, input);
    }
    let file = cached_file(inode)?;
    let _operation = file.operation.lock();
    let written = file.inode.write_storage(offset, input)?;
    file.update_cached(offset, &input[..written])?;
    Ok(written)
}

/// @description 在 page-cache operation lock 内原子执行受最大文件大小约束的 append。
///
/// @param inode 目标 inode。
/// @param input 待追加数据。
/// @param size_limit caller 的 RLIMIT_FSIZE soft limit。
/// @return append 起始 offset 与实际字节数；已到上限时返回零字节，由 syscall 生成 SIGXFSZ/EFBIG。
pub(crate) fn append(
    inode: Arc<dyn Inode>,
    input: &[u8],
    size_limit: u64,
) -> Result<(u64, usize), FileSystemError> {
    if inode.inode_type() != InodeType::File {
        return inode.append_storage(input);
    }
    let file = cached_file(inode)?;
    let _operation = file.operation.lock();
    let offset = file.inode.size();
    let allowed = usize::try_from(size_limit.saturating_sub(offset))
        .unwrap_or(usize::MAX)
        .min(input.len());
    if allowed == 0 {
        return Ok((offset, 0));
    }
    let (offset, written) = file.inode.append_storage(&input[..allowed])?;
    file.update_cached(offset, &input[..written])?;
    Ok((offset, written))
}

pub(crate) fn truncate(inode: Arc<dyn Inode>, size: u64) -> Result<(), FileSystemError> {
    if inode.inode_type() != InodeType::File {
        return inode.truncate_storage(size);
    }
    let file = cached_file(inode)?;
    let _operation = file.operation.lock();
    file.inode.truncate_storage(size)?;
    let first_removed = size.div_ceil(PAGE_SIZE as u64);
    let mut pages = file.pages.lock();
    pages.retain(|index, _| *index < first_removed);
    if !size.is_multiple_of(PAGE_SIZE as u64)
        && let Some(page) = pages.get(&(size / PAGE_SIZE as u64))
    {
        page.frame.zero_from(size as usize % PAGE_SIZE);
    }
    drop(pages);
    drop(_operation);
    invalidate_shared_file(file.id, size).map_err(shared_error)
}

pub(crate) fn sync_inode(inode: Arc<dyn Inode>) -> Result<(), FileSystemError> {
    if inode.inode_type() != InodeType::File {
        return inode.sync_storage();
    }
    let file = cached_file(inode)?;
    let _operation = file.operation.lock();
    file.writeback_range(0, u64::MAX)
}

pub(crate) fn sync_all() -> Result<(), FileSystemError> {
    let files: Vec<_> = FILES
        .call_once(|| Mutex::new(BTreeMap::new()))
        .lock()
        .values()
        .cloned()
        .collect();
    for file in files {
        let _operation = file.operation.lock();
        file.writeback_range(0, u64::MAX)?;
    }
    FILES
        .wait()
        .lock()
        .retain(|_, file| Arc::strong_count(file) != 1 || !file.pages.lock().is_empty());
    Ok(())
}

fn fs_error(error: FileSystemError) -> SharedFileError {
    match error {
        FileSystemError::OutOfMemory => SharedFileError::OutOfMemory,
        FileSystemError::InvalidOperation => SharedFileError::BeyondEof,
        _ => SharedFileError::Io,
    }
}

fn shared_error(error: SharedFileError) -> FileSystemError {
    match error {
        SharedFileError::OutOfMemory => FileSystemError::OutOfMemory,
        SharedFileError::BeyondEof => FileSystemError::InvalidOperation,
        SharedFileError::Io => FileSystemError::IoError,
    }
}
