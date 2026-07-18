use super::{
    FileSystemError, PAGE_SIZE, RegularFileWrite, WRITEBACK_BATCH_PAGES,
    writeback_batch::commit_contiguous_prefix_with_backoff,
};

impl RegularFileWrite<'_> {
    /// 一次 regular write transient staging 的硬上限。
    ///
    /// 32 个 logical pages 与 page-cache writeback 共用同一 transaction/backoff policy；
    /// 非对齐 byte range 可能触及 33 个 filesystem blocks，并由 storage capacity error 退避。
    pub(crate) const MAX_STAGING_BYTES: usize = WRITEBACK_BATCH_PAGES * PAGE_SIZE;

    fn write_batched(
        &self,
        input: &[u8],
        mut write: impl FnMut(usize, &[u8]) -> Result<(u64, usize), FileSystemError>,
    ) -> Result<(u64, usize), FileSystemError> {
        assert!(!input.is_empty());
        assert!(input.len() <= Self::MAX_STAGING_BYTES);
        commit_contiguous_prefix_with_backoff(
            input.len(),
            PAGE_SIZE,
            |start, count| write(start, &input[start..start + count]),
            |offset, start, written| {
                self.file
                    .update_cached(offset, &input[start..start + written]);
            },
            |error| *error == FileSystemError::NoSpace,
        )
        .map(|committed| (committed.offset, committed.bytes))
    }

    /// @description 向 regular-file storage 写入并同步更新 resident cache pages。
    /// @param offset 文件 byte offset。
    /// @param input kernel-owned 输入缓冲区。
    /// @return storage 实际写入字节数。
    /// @error storage mutation 失败时透传 filesystem error。
    pub(crate) fn write(&self, offset: u64, input: &[u8]) -> Result<usize, FileSystemError> {
        if input.is_empty() {
            return Ok(0);
        }
        let _operation = self.file.operation.lock();
        self.write_batched(input, |start, bytes| {
            let offset = offset
                .checked_add(start as u64)
                .ok_or(FileSystemError::NoSpace)?;
            self.file
                .inode
                .write_storage(offset, bytes)
                .map(|written| (offset, written))
        })
        .map(|(_, written)| written)
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
        let committed = self.write_batched(&input[..allowed], |_, bytes| {
            self.file.inode.append_storage(bytes)
        })?;
        assert_eq!(
            committed.0, offset,
            "operation-locked append changed placement before first transaction"
        );
        Ok(committed)
    }
}
