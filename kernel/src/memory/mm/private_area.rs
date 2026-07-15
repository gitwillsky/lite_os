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

/// @description private file/ELF VMA 的不可变 fault source；resident 私有页仍只由 MapArea 持有。
#[derive(Clone)]
pub(super) struct PrivateFileArea {
    source: PrivateSource,
}

/// @description private-file fault 在 private frame 分配前冻结的瞬时 backing snapshot。
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
    /// @description 为 PT_LOAD 建立按页随机读取的 backing，不复制完整 segment。
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

    /// @description 为 MAP_PRIVATE regular file 建立 page-cache read seam。
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

    fn cached_relative_page(data_start: usize, vpn: VirtualPageNumber) -> Option<u64> {
        let start = data_start / PAGE_SIZE;
        let delta = vpn.as_usize().checked_sub(start)?;
        u64::try_from(delta).ok()
    }

    /// @description 判断当前 fault page 是否仍有文件对象覆盖；truncate 后的整页返回 SIGBUS。
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

    /// @description 在 private frame allocation/reclaim 前稳定当前 fault page。
    /// @return cached page Arc 与 truncate 的 operation domain 线性化；EOF 不分配任何页。
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

    /// @description 判断 resident private page 是否仍包含文件数据，而非纯 BSS/EOF 零页。
    ///
    /// @param vpn 待分类的 VMA virtual page number。
    /// @return page 与 executable/file 数据区间存在非空交集时为 true。
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

    /// @description 投影 truncate 后首个必须撤销的 cached private VMA page。
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

    /// @description 从 allocation 前冻结的 backing snapshot 填充 fault page。
    pub(super) fn fill(
        &self,
        vpn: VirtualPageNumber,
        frame: &mut FrameTracker,
        prepared: &PrivateFaultPreparation,
    ) -> Result<(), MemoryError> {
        match (&self.source, prepared) {
            (PrivateSource::CachedFile { .. }, PrivateFaultPreparation::Cached(cached)) => {
                cached.frame().read(0, frame.bytes_mut());
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
