use alloc::sync::Arc;
use core::fmt;

use crate::memory::{
    ExecutableSource, PAGE_SIZE, SharedFileError, SharedFileMapping, SharedPage,
    address::VirtualPageNumber, frame_allocator::FrameTracker,
};

use super::{FilePageRange, MemoryError};

#[derive(Clone)]
enum PrivateSource {
    Executable {
        source: Arc<dyn ExecutableSource>,
        data_start: usize,
        source_offset: usize,
        file_size: usize,
    },
    CachedFile {
        source: Arc<dyn SharedFileMapping>,
        data_start: usize,
        pages: FilePageRange,
    },
}

/// private file/ELF VMA 的不可变 fault source；resident 私有页仍只由 MapArea 持有。
#[derive(Clone)]
pub(super) struct PrivateFileArea {
    source: PrivateSource,
}

/// private-file fault 在 private frame 分配前冻结的瞬时 backing snapshot。
pub(super) enum PrivateFaultPreparation {
    Executable,
    Cached(Arc<dyn SharedPage>),
    BeyondEof,
}

impl fmt::Debug for PrivateFileArea {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("PrivateFileArea");
        match &self.source {
            PrivateSource::Executable {
                data_start,
                source_offset,
                file_size,
                ..
            } => debug
                .field("kind", &"executable")
                .field("data_start", data_start)
                .field("source_offset", source_offset)
                .field("file_size", file_size),
            PrivateSource::CachedFile {
                data_start, pages, ..
            } => debug
                .field("kind", &"cached-file")
                .field("data_start", data_start)
                .field("pages", pages),
        };
        debug.finish_non_exhaustive()
    }
}

impl PrivateFileArea {
    /// 为 PT_LOAD 建立按页随机读取的 backing，不复制完整 segment。
    pub(super) fn executable(
        source: Arc<dyn ExecutableSource>,
        data_start: usize,
        source_offset: usize,
        file_size: usize,
    ) -> Self {
        Self {
            source: PrivateSource::Executable {
                source,
                data_start,
                source_offset,
                file_size,
            },
        }
    }

    /// 为 MAP_PRIVATE regular file 建立 page-cache read seam。
    pub(super) fn cached_file(
        source: Arc<dyn SharedFileMapping>,
        data_start: usize,
        pages: FilePageRange,
    ) -> Self {
        Self {
            source: PrivateSource::CachedFile {
                source,
                data_start,
                pages,
            },
        }
    }

    /// VMA 平移后，把以绝对地址记录的 data 起点同步平移；backing 内容本身不变。
    pub(super) fn rebased(&mut self, from: VirtualPageNumber, to: VirtualPageNumber) {
        // 模 2^64 算术：data_start 可以落在 VMA 起点之前（首页内偏移），逐步 checked 会误报溢出。
        let delta = to
            .as_usize()
            .wrapping_sub(from.as_usize())
            .wrapping_mul(PAGE_SIZE);
        match &mut self.source {
            PrivateSource::Executable { data_start, .. }
            | PrivateSource::CachedFile { data_start, .. } => {
                *data_start = data_start.wrapping_add(delta);
            }
        }
    }

    /// 判断 VMA 末尾追加 `extra` 页后，文件页范围是否仍合法。
    pub(super) fn can_extend(&self, extra: u64) -> bool {
        match &self.source {
            // ELF 超出文件尾的部分按零页填充，不需要扩展范围。
            PrivateSource::Executable { .. } => true,
            PrivateSource::CachedFile { pages, .. } => pages.extended(extra).is_some(),
        }
    }

    /// 在 `can_extend` 成功后真正扩展；追加的页越过文件尾时缺页返回 SIGBUS。
    pub(super) fn extend(&mut self, extra: u64) {
        if let PrivateSource::CachedFile { pages, .. } = &mut self.source {
            *pages = pages.extended(extra).expect("extension was validated");
        }
    }

    fn cached_relative_page(data_start: usize, vpn: VirtualPageNumber) -> Option<u64> {
        let start = data_start / PAGE_SIZE;
        let delta = vpn.as_usize().checked_sub(start)?;
        u64::try_from(delta).ok()
    }

    /// 判断当前 fault page 是否仍有文件对象覆盖；truncate 后的整页返回 SIGBUS。
    pub(super) fn faultable(&self, vpn: VirtualPageNumber) -> Result<bool, MemoryError> {
        match &self.source {
            PrivateSource::Executable { .. } => Ok(true),
            PrivateSource::CachedFile {
                source,
                data_start,
                pages,
            } => Self::cached_relative_page(*data_start, vpn)
                .and_then(|page| pages.has_file_bytes(page, source.size()))
                .ok_or(MemoryError::InvalidRange),
        }
    }

    /// 在 private frame allocation/reclaim 前稳定当前 fault page。
    ///
    /// # Returns
    ///
    /// cached page Arc 与 truncate 的 operation domain 线性化；EOF 不分配任何页。
    pub(super) fn prepare_fault(
        &self,
        vpn: VirtualPageNumber,
    ) -> Result<PrivateFaultPreparation, MemoryError> {
        match &self.source {
            PrivateSource::Executable { .. } => Ok(PrivateFaultPreparation::Executable),
            PrivateSource::CachedFile {
                source,
                data_start,
                pages,
            } => {
                let relative = Self::cached_relative_page(*data_start, vpn)
                    .ok_or(MemoryError::InvalidRange)?;
                let page = pages.page(relative).ok_or(MemoryError::InvalidRange)?;
                match source.page(page) {
                    Ok(page) => Ok(PrivateFaultPreparation::Cached(page)),
                    Err(SharedFileError::BeyondEof) => Ok(PrivateFaultPreparation::BeyondEof),
                    Err(SharedFileError::OutOfMemory) => Err(MemoryError::OutOfMemory),
                    Err(SharedFileError::Io) => Err(MemoryError::Io),
                }
            }
        }
    }

    /// 判断 resident private page 是否仍包含文件数据，而非纯 BSS/EOF 零页。
    ///
    /// # Parameters
    ///
    /// - `vpn`: 待分类的 VMA virtual page number。
    ///
    /// # Returns
    ///
    /// page 与 executable/file 数据区间存在非空交集时为 true。
    pub(super) fn has_file_bytes(&self, vpn: VirtualPageNumber) -> bool {
        match &self.source {
            PrivateSource::Executable {
                data_start,
                file_size,
                ..
            } => {
                let page_start = vpn
                    .as_usize()
                    .checked_mul(PAGE_SIZE)
                    .expect("executable VMA page address overflow");
                let page_end = page_start
                    .checked_add(PAGE_SIZE)
                    .expect("executable VMA page end overflow");
                let data_end = data_start
                    .checked_add(*file_size)
                    .expect("validated executable file range overflow");
                page_start < data_end && *data_start < page_end
            }
            PrivateSource::CachedFile {
                source,
                data_start,
                pages,
            } => Self::cached_relative_page(*data_start, vpn)
                .and_then(|page| pages.has_file_bytes(page, source.size()))
                .expect("cached-file VMA escaped its validated page range"),
        }
    }

    /// 投影 truncate 后首个必须撤销的 cached private VMA page。
    pub(super) fn first_stale_page(
        &self,
        vma_start: VirtualPageNumber,
        mapping_id: crate::memory::SharedFileId,
        file_size: u64,
    ) -> Option<VirtualPageNumber> {
        let PrivateSource::CachedFile {
            source,
            data_start,
            pages,
        } = &self.source
        else {
            return None;
        };
        if source.id() != mapping_id {
            return None;
        }
        pages
            .stale_resident_start(*data_start / PAGE_SIZE, vma_start.as_usize(), file_size)
            .map(VirtualPageNumber::from_vpn)
    }

    /// 从 allocation 前冻结的 backing snapshot 填充 fault page。
    pub(super) fn fill(
        &self,
        vpn: VirtualPageNumber,
        frame: &mut FrameTracker,
        prepared: &PrivateFaultPreparation,
    ) -> Result<(), MemoryError> {
        match (&self.source, prepared) {
            (PrivateSource::CachedFile { .. }, PrivateFaultPreparation::Cached(cached)) => {
                cached.pin()?.read(0, frame.bytes_mut());
                Ok(())
            }
            (
                PrivateSource::Executable {
                    source,
                    data_start,
                    source_offset,
                    file_size,
                },
                PrivateFaultPreparation::Executable,
            ) => {
                let page_start = vpn
                    .as_usize()
                    .checked_mul(PAGE_SIZE)
                    .ok_or(MemoryError::InvalidRange)?;
                let page_end = page_start
                    .checked_add(PAGE_SIZE)
                    .ok_or(MemoryError::InvalidRange)?;
                let data_end = data_start
                    .checked_add(*file_size)
                    .ok_or(MemoryError::InvalidRange)?;
                let start = page_start.max(*data_start);
                let end = page_end.min(data_end);
                if start >= end {
                    return Ok(());
                }
                let source_offset = source_offset
                    .checked_add(start - *data_start)
                    .ok_or(MemoryError::InvalidRange)?;
                let output = &mut frame.bytes_mut()[start - page_start..end - page_start];
                source
                    .read_exact_at(source_offset, output)
                    .map_err(|_| MemoryError::Io)
            }
            (_, PrivateFaultPreparation::BeyondEof)
            | (PrivateSource::Executable { .. }, PrivateFaultPreparation::Cached(_))
            | (PrivateSource::CachedFile { .. }, PrivateFaultPreparation::Executable) => {
                Err(MemoryError::InvalidRange)
            }
        }
    }
}
