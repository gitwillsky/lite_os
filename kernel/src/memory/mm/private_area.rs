use alloc::sync::Arc;
use core::fmt;

use crate::memory::{
    ExecutableSource, PAGE_SIZE, SharedFileError, SharedFileMapping, address::VirtualPageNumber,
    frame_allocator::FrameTracker,
};

use super::MemoryError;

#[derive(Clone)]
enum PrivateSource {
    Executable(Arc<dyn ExecutableSource>),
    CachedFile(Arc<dyn SharedFileMapping>),
}

/// @description private file/ELF VMA 的不可变 fault source；resident 私有页仍只由 MapArea 持有。
#[derive(Clone)]
pub(super) struct PrivateFileArea {
    source: PrivateSource,
    data_start: usize,
    source_offset: usize,
    file_size: usize,
}

impl fmt::Debug for PrivateFileArea {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivateFileArea")
            .field("data_start", &self.data_start)
            .field("source_offset", &self.source_offset)
            .field("file_size", &self.file_size)
            .finish_non_exhaustive()
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
            source: PrivateSource::Executable(source),
            data_start,
            source_offset,
            file_size,
        }
    }

    /// @description 为 MAP_PRIVATE regular file 建立 page-cache read seam。
    pub(super) fn cached_file(
        source: Arc<dyn SharedFileMapping>,
        data_start: usize,
        source_offset: usize,
    ) -> Self {
        Self {
            source: PrivateSource::CachedFile(source),
            data_start,
            source_offset,
            file_size: 0,
        }
    }

    /// @description 判断当前 fault page 是否仍有文件对象覆盖；truncate 后的整页返回 SIGBUS。
    pub(super) fn faultable(&self, vpn: VirtualPageNumber) -> bool {
        match &self.source {
            PrivateSource::Executable(_) => true,
            PrivateSource::CachedFile(source) => {
                let page_start = vpn.as_usize().saturating_mul(PAGE_SIZE);
                let offset = self
                    .source_offset
                    .saturating_add(page_start.saturating_sub(self.data_start));
                (offset as u64) < source.size()
            }
        }
    }

    /// @description 只填充 fault page 与文件数据区间的交集；BSS/EOF 后区域保持零。
    pub(super) fn fill(
        &self,
        vpn: VirtualPageNumber,
        frame: &mut FrameTracker,
    ) -> Result<(), MemoryError> {
        let page_start = vpn
            .as_usize()
            .checked_mul(PAGE_SIZE)
            .ok_or(MemoryError::InvalidRange)?;
        let page_end = page_start + PAGE_SIZE;
        let file_size = match &self.source {
            PrivateSource::Executable(_) => self.file_size,
            PrivateSource::CachedFile(source) => {
                usize::try_from(source.size().saturating_sub(self.source_offset as u64))
                    .unwrap_or(usize::MAX)
            }
        };
        let data_end = self
            .data_start
            .checked_add(file_size)
            .ok_or(MemoryError::InvalidRange)?;
        let start = page_start.max(self.data_start);
        let end = page_end.min(data_end);
        if start >= end {
            return Ok(());
        }
        let source_offset = self
            .source_offset
            .checked_add(start - self.data_start)
            .ok_or(MemoryError::InvalidRange)?;
        let output = &mut frame.bytes_mut()[start - page_start..end - page_start];
        match &self.source {
            PrivateSource::Executable(source) => source
                .read_exact_at(source_offset, output)
                .map_err(|_| MemoryError::Io),
            PrivateSource::CachedFile(source) => {
                read_cached(source.as_ref(), source_offset, output)
            }
        }
    }
}

fn read_cached(
    source: &dyn SharedFileMapping,
    offset: usize,
    output: &mut [u8],
) -> Result<(), MemoryError> {
    let end = (offset as u64)
        .checked_add(output.len() as u64)
        .filter(|end| *end <= source.size())
        .ok_or(MemoryError::Io)?;
    let mut current = offset as u64;
    let mut copied = 0;
    while current < end {
        let page = source
            .page(current / PAGE_SIZE as u64)
            .map_err(|error| match error {
                SharedFileError::OutOfMemory => MemoryError::OutOfMemory,
                SharedFileError::Io | SharedFileError::BeyondEof => MemoryError::Io,
            })?;
        let page_offset = current as usize % PAGE_SIZE;
        let count = (PAGE_SIZE - page_offset).min(output.len() - copied);
        page.frame()
            .read(page_offset, &mut output[copied..copied + count]);
        current += count as u64;
        copied += count;
    }
    Ok(())
}
