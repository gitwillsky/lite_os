use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::{Mutex, MutexGuard, Once};

use crate::memory::{
    MemoryReclaimer, PAGE_SIZE, SharedFileError, SharedFileId, SharedFileMapping, SharedFrame,
    SharedPage, invalidate_shared_file, register_memory_reclaimer,
};

use super::{FileSystemError, Inode, InodeType};

const PAGE_DIRTY: usize = 1 << (usize::BITS - 1);
const PAGE_WRITER_MASK: usize = PAGE_DIRTY - 1;
const WRITEBACK_BATCH_PAGES: usize = 32;

#[derive(Debug)]
struct CachedPage {
    frame: SharedFrame,
    // OWNER: CachedPage 用一个 CAS domain 同时拥有 dirty bit 与 writable-PTE 引用计数。
    // 拆成两个 Atomic 会让 writeback 的 writer==0 / clear-dirty 间隙吞掉并发 writer publication。
    state: AtomicUsize,
}

impl CachedPage {
    fn dirty(&self) -> bool {
        self.state.load(Ordering::Acquire) & PAGE_DIRTY != 0
    }

    fn reclaimable(&self) -> bool {
        self.state.load(Ordering::Acquire) == 0
    }

    fn mark_clean_if_unmapped(&self) {
        // writer acquire 与本 CAS 竞争：先 acquire 则 writer count 阻止清理，后 acquire
        // 则重新原子发布 dirty，因而不存在已映射 writable page 被误标 clean 的窗口。
        let _ = self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & PAGE_WRITER_MASK == 0).then_some(0)
            });
    }
}

impl SharedPage for CachedPage {
    fn frame(&self) -> &SharedFrame {
        &self.frame
    }

    fn acquire_writer(&self) {
        let result = self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & PAGE_WRITER_MASK)
                    .checked_add(1)
                    .filter(|writers| *writers <= PAGE_WRITER_MASK)
                    .map(|writers| PAGE_DIRTY | writers)
            });
        assert!(result.is_ok(), "shared-page writer count overflow");
    }

    fn release_writer(&self) {
        self.state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                let writers = state & PAGE_WRITER_MASK;
                (writers != 0).then_some((state & PAGE_DIRTY) | (writers - 1))
            })
            .expect("shared-page writer release without acquire");
    }
}

struct CachedFile {
    id: SharedFileId,
    inode: Arc<dyn Inode>,
    // OWNER: 单 inode operation lock 串行化 cache fill、write/append、truncate 与 writeback；
    // 缺失 fill 归属会让并发旧 storage read 在 write/truncate 后插入 stale cache page。
    operation: Mutex<()>,
    // OWNER: 单 inode write_sequence gate 排序完整 regular write、truncate、allocate 与 fd/global sync；
    // 缺失时 512-byte user-copy chunks 可被另一 OFD write 穿插，破坏 writev/append 连续性。
    // page fault 不获取该 gate，只取内层 operation，因此同文件 mmap buffer fault 不会自死锁。
    write_sequence: Mutex<()>,
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
        // 1. miss fill 与 storage mutation 共用 operation domain，保证读出的 bytes 和最终
        // cache publication 之间没有 write/truncate 可以穿过。
        let _operation = self.operation.lock();
        // 2. 等锁期间另一个 filler 可能已经发布同一 page；必须复查，否则会重复 I/O，
        // 并让后完成者覆盖先完成者的唯一 cache owner。
        if let Some(page) = self.pages.lock().get(&index).cloned() {
            return Ok(page);
        }
        let mut frame = SharedFrame::allocate().map_err(shared_error)?;
        let offset = index
            .checked_mul(PAGE_SIZE as u64)
            .ok_or(FileSystemError::InvalidOperation)?;
        let size = self.inode.size();
        if offset >= size {
            return Err(FileSystemError::InvalidOperation);
        }
        let available = usize::try_from(size - offset)
            .unwrap_or(usize::MAX)
            .min(PAGE_SIZE);
        // 3. frame 尚未发布且保持独占，storage 直接填充其有效前缀；临时 Vec 会在每次
        // cache miss 增加一次 heap allocation 和一次最多整页 memcpy。
        let read = self
            .inode
            .read_storage(offset, &mut frame.bytes_mut()[..available])?;
        if read != available {
            return Err(FileSystemError::IoError);
        }
        let page = Arc::new(CachedPage {
            frame,
            state: AtomicUsize::new(0),
        });
        let mut pages = self.pages.lock();
        assert!(pages.insert(index, page.clone()).is_none());
        Ok(page)
    }

    fn update_cached(&self, offset: u64, input: &[u8]) {
        if input.is_empty() {
            return;
        }
        // 1. storage adapter 已确认完整写区间；overflow 表示其返回值破坏内部契约，必须 fail-stop。
        let end = offset
            .checked_add(input.len() as u64)
            .expect("storage write returned an overflowing byte range");
        let first = offset / PAGE_SIZE as u64;
        let last = (end - 1) / PAGE_SIZE as u64;
        let pages = self.pages.lock();
        // 2. 从 page index 反向遍历实际 resident pages，跳过大写入区间中的 cache holes。
        for (&index, page) in pages.range(first..=last) {
            let page_start = index * PAGE_SIZE as u64;
            let copy_start = offset.max(page_start);
            let copy_end = end.min(page_start.saturating_add(PAGE_SIZE as u64));
            // 3. 首尾 page 只覆盖交集；中间 page 自然复制完整 PAGE_SIZE。
            let source_start = (copy_start - offset) as usize;
            let page_offset = (copy_start - page_start) as usize;
            let count = (copy_end - copy_start) as usize;
            page.frame
                .write(page_offset, &input[source_start..source_start + count]);
        }
    }

    fn writeback_range(&self, offset: u64, length: u64) -> Result<(), FileSystemError> {
        let end = offset.saturating_add(length);
        // 1. operation lock 保证本次 writeback 的 EOF 不被 write/truncate 改动；只读取一次，
        // 避免每个 resident page 都进入 filesystem metadata owner。
        let size = self.inode.size();
        // 2. 每批最多扫描固定数量 resident pages 并只 clone dirty Arc；range-sized Vec 会让
        // 大 mapping 的 munmap 在内存压力下反而申请连续大块 heap 并 panic。
        let first = offset / PAGE_SIZE as u64;
        let last = end.saturating_sub(1) / PAGE_SIZE as u64;
        let mut next = first;
        let mut data = [0u8; PAGE_SIZE];
        loop {
            let mut batch: [Option<(u64, Arc<CachedPage>)>; WRITEBACK_BATCH_PAGES] =
                core::array::from_fn(|_| None);
            let mut scanned = 0usize;
            let mut dirty = 0usize;
            {
                let pages = self.pages.lock();
                for (&index, page) in pages.range(next..=last) {
                    next = index
                        .checked_add(1)
                        .expect("cached page index cannot reach u64::MAX");
                    scanned += 1;
                    if page.dirty() {
                        batch[dirty] = Some((index, page.clone()));
                        dirty += 1;
                    }
                    if scanned == WRITEBACK_BATCH_PAGES {
                        break;
                    }
                }
            }
            if scanned == 0 {
                break;
            }
            let reached_end = next > last;
            // 3. 单一 stack scratch 跨 batch 复用；storage 成功后仅在没有 writable PTE 时
            // CAS 清 dirty，错误路径保留当前及后续 page 的 dirty 状态供下一次 sync 重试。
            for entry in &mut batch[..dirty] {
                let (index, page) = entry.take().expect("dirty writeback slot must exist");
                let page_start = index * PAGE_SIZE as u64;
                let count = usize::try_from(size.saturating_sub(page_start))
                    .unwrap_or(usize::MAX)
                    .min(PAGE_SIZE);
                if count == 0 {
                    continue;
                }
                page.frame.read(0, &mut data[..count]);
                if self.inode.write_storage(page_start, &data[..count])? != count {
                    return Err(FileSystemError::IoError);
                }
                page.mark_clean_if_unmapped();
            }
            if reached_end {
                break;
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
        // AddressSpace owner 在调用 mapping sync 时已持 mm lock；这里只取内层 operation，
        // 否则会与 write_sequence → user-copy(AddressSpace) 形成反向锁序。
        let _operation = self.operation.lock();
        self.writeback_range(offset, length).map_err(fs_error)
    }
}

impl MemoryReclaimer for CachedFile {
    fn reclaim_pages(&self, limit: usize) -> usize {
        let Some(mut pages) = self.pages.try_lock() else {
            return 0;
        };
        // 1. extract_if 只移除无外部引用的 clean page；dirty/writer owner 继续留在 cache。
        // 2. take 达到 quota 后立即 drop iterator，未访问 entry 保留。若使用 retain，即使
        // 已回收足额页仍会扫描整个大文件 cache，把一次 OOM recovery 退化为 O(cache pages)。
        pages
            .extract_if(.., |_, page| {
                page.reclaimable() && Arc::strong_count(page) == 1
            })
            .take(limit)
            .count()
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
        write_sequence: Mutex::new(()),
        pages: Mutex::new(BTreeMap::new()),
    });
    register_memory_reclaimer(file.clone()).map_err(shared_error)?;
    files.insert(id, file.clone());
    Ok(file)
}

/// @description 已解析 page-cache identity 的 regular-file I/O facade。
///
/// syscall 在一次 read/write 操作内复用该值，避免每个 user-copy chunk 重复读取 inode
/// metadata、获取全局 FILES lock 并查找同一个 BTree entry。
pub(crate) struct RegularFile(Arc<CachedFile>);

/// @description 持有单 inode write-sequence ownership 的一次 regular-file mutation。
///
/// Drop 无条件释放 gate；error、signal 或 partial user-copy 都不会遗留 transaction owner。
pub(crate) struct RegularFileWrite<'a> {
    file: &'a CachedFile,
    _sequence: MutexGuard<'a, ()>,
}

impl RegularFile {
    /// @description 将 regular inode 解析为唯一 page-cache owner。
    /// @param inode 目标 regular inode。
    /// @return 可在本次 I/O 内复用的 facade。
    /// @error `InvalidOperation` 表示 inode 不是 regular file。
    /// @error inode metadata 读取失败时透传 filesystem error。
    /// @error 首次注册 memory reclaimer 失败时返回 `OutOfMemory`。
    pub(crate) fn from_inode(inode: Arc<dyn Inode>) -> Result<Self, FileSystemError> {
        cached_file(inode).map(Self)
    }

    /// @description 从 page cache 读取 regular-file bytes。
    /// @param offset 文件 byte offset。
    /// @param output kernel-owned 输出缓冲区。
    /// @return 实际读取字节数；EOF 返回零。
    /// @error cache fill 分配失败时返回 `OutOfMemory`。
    /// @error size snapshot 后并发 truncate 越过当前 page 时返回 `InvalidOperation`。
    /// @error storage read 失败或短读时返回对应 filesystem error。
    pub(crate) fn read(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
        let size = self.0.inode.size();
        let count = usize::try_from(size.saturating_sub(offset))
            .unwrap_or(usize::MAX)
            .min(output.len());
        let mut done = 0;
        while done < count {
            let current = offset + done as u64;
            let page = self.0.page(current / PAGE_SIZE as u64)?;
            let page_offset = current as usize % PAGE_SIZE;
            let part = (PAGE_SIZE - page_offset).min(count - done);
            page.frame.read(page_offset, &mut output[done..done + part]);
            done += part;
        }
        Ok(done)
    }

    /// @description 开始一次不可被其他 regular-file mutation 穿插的 write operation。
    /// @return 持有 per-inode write-sequence gate 的 mutation facade；Drop 自动释放。
    pub(crate) fn begin_write(&self) -> RegularFileWrite<'_> {
        RegularFileWrite {
            file: &self.0,
            _sequence: self.0.write_sequence.lock(),
        }
    }
}

impl RegularFileWrite<'_> {
    /// @description 向 regular-file storage 写入并同步更新 resident cache pages。
    /// @param offset 文件 byte offset。
    /// @param input kernel-owned 输入缓冲区。
    /// @return storage 实际写入字节数。
    /// @error storage mutation 失败时透传 filesystem error。
    pub(crate) fn write(&self, offset: u64, input: &[u8]) -> Result<usize, FileSystemError> {
        let _operation = self.file.operation.lock();
        let written = self.file.inode.write_storage(offset, input)?;
        self.file.update_cached(offset, &input[..written]);
        Ok(written)
    }

    /// @description 在 page-cache operation lock 内原子执行受最大文件大小约束的 append。
    /// @param input 待追加数据。
    /// @param size_limit caller 的 RLIMIT_FSIZE soft limit。
    /// @return append 起始 offset 与实际字节数；已到上限时返回零字节，由 syscall 生成 SIGXFSZ/EFBIG。
    /// @error storage mutation 失败时透传 filesystem error。
    pub(crate) fn append(
        &self,
        input: &[u8],
        size_limit: u64,
    ) -> Result<(u64, usize), FileSystemError> {
        let _operation = self.file.operation.lock();
        let offset = self.file.inode.size();
        let allowed = usize::try_from(size_limit.saturating_sub(offset))
            .unwrap_or(usize::MAX)
            .min(input.len());
        if allowed == 0 {
            return Ok((offset, 0));
        }
        let (offset, written) = self.file.inode.append_storage(&input[..allowed])?;
        self.file.update_cached(offset, &input[..written]);
        Ok((offset, written))
    }
}

pub(crate) fn mapping(
    inode: Arc<dyn Inode>,
) -> Result<Arc<dyn SharedFileMapping>, FileSystemError> {
    cached_file(inode).map(|file| file as Arc<dyn SharedFileMapping>)
}

pub(crate) fn truncate(inode: Arc<dyn Inode>, size: u64) -> Result<(), FileSystemError> {
    if inode.inode_type() != InodeType::File {
        return inode.truncate_storage(size);
    }
    let file = cached_file(inode)?;
    let _sequence = file.write_sequence.lock();
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

/// @description 在 page-cache operation domain 内预分配 regular-file backing blocks。
/// @param inode 目标 regular inode。
/// @param offset byte range 起点。
/// @param length 非零 byte range 长度。
/// @return allocation 与可能的 size extension 完成；cached contents 保持不变。
pub(crate) fn allocate(
    inode: Arc<dyn Inode>,
    offset: u64,
    length: u64,
) -> Result<(), FileSystemError> {
    if inode.inode_type() != InodeType::File {
        return inode.allocate_storage(offset, length);
    }
    let file = cached_file(inode)?;
    let _sequence = file.write_sequence.lock();
    let _operation = file.operation.lock();
    file.inode.allocate_storage(offset, length)
}

pub(crate) fn sync_inode(inode: Arc<dyn Inode>) -> Result<(), FileSystemError> {
    if inode.inode_type() != InodeType::File {
        return inode.sync_storage();
    }
    let file = cached_file(inode)?;
    let _sequence = file.write_sequence.lock();
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
        let _sequence = file.write_sequence.lock();
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
