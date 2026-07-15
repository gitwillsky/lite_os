/// Linux/RV64 regular-file mmap page range construction failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FilePageRangeError {
    /// mmap length is zero or the file offset is not page aligned.
    Invalid,
    /// Rounded length or the signed Linux file-position range overflows.
    Overflow,
}

/// Immutable file-page interval validated before a VMA transaction starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FilePageRange {
    start: u64,
    count: u64,
}

impl FilePageRange {
    /// Validate one regular-file mmap offset and byte length.
    pub(super) fn new(offset: u64, length: usize) -> Result<Self, FilePageRangeError> {
        let page_size = crate::memory::PAGE_SIZE as u64;
        if length == 0 || !offset.is_multiple_of(page_size) {
            return Err(FilePageRangeError::Invalid);
        }
        let length = u64::try_from(length).map_err(|_| FilePageRangeError::Overflow)?;
        let count = length.div_ceil(page_size);
        let rounded_length = count
            .checked_mul(page_size)
            .ok_or(FilePageRangeError::Overflow)?;
        let max_file_size = i64::MAX as u64;
        if rounded_length > max_file_size {
            return Err(FilePageRangeError::Overflow);
        }
        if offset > max_file_size - rounded_length {
            return Err(FilePageRangeError::Overflow);
        }
        Ok(Self {
            start: offset / page_size,
            count,
        })
    }

    /// First file page owned by this interval.
    const fn start(self) -> u64 {
        self.start
    }

    /// Number of pages in this interval.
    pub(super) const fn count(self) -> u64 {
        self.count
    }

    /// Derive a nonempty page interval contained by this validated range.
    pub(super) fn subrange(self, first: u64, count: u64) -> Option<Self> {
        if count == 0 || first > self.count || count > self.count - first {
            return None;
        }
        Some(Self {
            start: self.start.checked_add(first)?,
            count,
        })
    }

    /// Project one relative VMA page to its regular-file page index.
    pub(super) fn page(self, relative: u64) -> Option<u64> {
        (relative < self.count)
            .then(|| self.start.checked_add(relative))
            .flatten()
    }

    /// Project one relative VMA page to its page-aligned file byte offset.
    pub(super) fn byte_offset(self, relative: u64) -> Option<u64> {
        self.page(relative)?
            .checked_mul(crate::memory::PAGE_SIZE as u64)
    }

    /// Project this complete page interval to a checked byte interval.
    pub(super) fn byte_range(self) -> Option<(u64, u64)> {
        Some((
            self.start().checked_mul(crate::memory::PAGE_SIZE as u64)?,
            self.count.checked_mul(crate::memory::PAGE_SIZE as u64)?,
        ))
    }

    /// Project a byte within one relative page to a stable file byte identity.
    pub(super) fn byte_within(self, relative: u64, within_page: usize) -> Option<u64> {
        if within_page >= crate::memory::PAGE_SIZE {
            return None;
        }
        self.byte_offset(relative)?
            .checked_add(u64::try_from(within_page).ok()?)
    }

    /// Count the prefix whose page starts still precede the current file EOF.
    pub(super) fn prefix_before(self, file_size: u64) -> Option<u64> {
        let start = self.start().checked_mul(crate::memory::PAGE_SIZE as u64)?;
        if file_size <= start {
            return Some(0);
        }
        Some(
            (file_size - start)
                .div_ceil(crate::memory::PAGE_SIZE as u64)
                .min(self.count),
        )
    }

    /// Project the first stale virtual page for a split view of this file range.
    pub(super) fn stale_resident_start(
        self,
        mapping_start: usize,
        resident_start: usize,
        file_size: u64,
    ) -> Option<usize> {
        let prefix = usize::try_from(self.prefix_before(file_size)?).ok()?;
        mapping_start
            .checked_add(prefix)
            .map(|vpn| vpn.max(resident_start))
    }

    /// Classify whether a mapped page begins before the current file EOF.
    pub(super) fn has_file_bytes(self, relative: u64, file_size: u64) -> Option<bool> {
        self.byte_offset(relative).map(|offset| offset < file_size)
    }
}
