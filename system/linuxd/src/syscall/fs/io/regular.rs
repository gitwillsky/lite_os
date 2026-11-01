use super::*;

/// 一次 regular-file read 跨 vector 的进度：同步循环与 io_uring 的异步读共用同一套“每块交付与收尾”规则。
///
/// chunk 取 `PAGE_SIZE`，同时匹配 page-cache frame 与 user-copy fault/copy 粒度；保持 512-byte staging
/// 会让满页顺序 I/O 重复八次 facade dispatch、地址计算和 page walk。
#[derive(Default)]
pub(in crate::syscall) struct ReadProgress {
    index: usize,
    done: usize,
    total: usize,
}

impl ReadProgress {
    /// 下一块要读的字节数（不超过一页）；全部 vector 读完返回 `None`。
    pub(in crate::syscall) fn next_count(&mut self, vectors: &[UserIoVec]) -> Option<usize> {
        while self.index < vectors.len() && self.done >= vectors[self.index].length {
            self.index += 1;
            self.done = 0;
        }
        vectors
            .get(self.index)
            .map(|vector| crate::memory::PAGE_SIZE.min(vector.length - self.done))
    }

    /// 读取失败：已有进度则返回 partial count，否则返回错误。
    pub(in crate::syscall) fn failed(&self, error: crate::fs::FileSystemError) -> isize {
        if self.total == 0 {
            ferr(error)
        } else {
            self.total as isize
        }
    }

    /// 把读到的 `chunk[..bytes]`（请求了 `count` 字节）交付给用户。
    ///
    /// offset 只在完整 copyout 后推进，坏 user range 不得消费未交付的文件 bytes；regular cache 仅会在
    /// EOF 产生 short read，所以 short read 之后不得跳到后续 vector 越过 EOF。
    ///
    /// # Returns
    ///
    /// `Some(result)` 表示读取到此结束，`None` 表示继续下一块。
    pub(in crate::syscall) fn deliver(
        &mut self,
        task: &TaskControlBlock,
        vectors: &[UserIoVec],
        position: &mut u64,
        chunk: &[u8],
        bytes: usize,
        count: usize,
    ) -> Option<isize> {
        if bytes == 0 {
            return Some(self.total as isize);
        }
        let partial = |total: usize| {
            Some(if total == 0 {
                -errno::EFAULT
            } else {
                total as isize
            })
        };
        let Some(address) = vectors[self.index].base.checked_add(self.done) else {
            return partial(self.total);
        };
        if task.copy_to_user(address, &chunk[..bytes]).is_err() {
            return partial(self.total);
        }
        *position += bytes as u64;
        self.done += bytes;
        self.total += bytes;
        (bytes < count).then_some(self.total as isize)
    }

    pub(in crate::syscall) fn total(&self) -> isize {
        self.total as isize
    }
}

/// 将一个或多个 userspace vector 作为一次 contiguous regular-file read 执行。
///
/// # Parameters
///
/// - `task`: userspace address owner。
/// - `file`: 已解析 page-cache identity 的 regular file。
/// - `position`: 本次操作唯一 byte offset；仅在成功 copyout 后推进。
/// - `vectors`: 按序消费的 userspace buffers。
///
/// # Returns
///
/// 总读取字节数、EOF 零、首错负 errno 或已有进度后的 partial count。
pub(super) fn read_vectors(
    task: &TaskControlBlock,
    ofd: &OpenFileDescription,
    file: &RegularFile,
    position: &mut u64,
    vectors: &[UserIoVec],
) -> isize {
    let _read_access = (file.tracks_access_time()
        && vectors.iter().any(|vector| vector.length != 0))
    .then(|| ofd.read_access());
    read_vectors_from(task, file, position, vectors, ReadProgress::default())
}

/// [`read_vectors`] 的阻塞循环，可从已有进度继续（io_uring 的异步读退回阻塞车道时使用）。
pub(in crate::syscall) fn read_vectors_from(
    task: &TaskControlBlock,
    file: &RegularFile,
    position: &mut u64,
    vectors: &[UserIoVec],
    mut progress: ReadProgress,
) -> isize {
    let mut chunk = [0u8; crate::memory::PAGE_SIZE];
    while let Some(count) = progress.next_count(vectors) {
        let read = match file.read(*position, &mut chunk[..count]) {
            Ok(read) => read,
            Err(error) => return progress.failed(error),
        };
        task.account_read_storage(read.storage_bytes);
        if let Some(result) = progress.deliver(task, vectors, position, &chunk, read.bytes, count) {
            return result;
        }
    }
    progress.total()
}

/// regular write 的 syscall-local transient staging owner。
///
/// `prepare` 必须在 OFD position/write-sequence gate 外调用，`with_prepared_staging`
/// 则保证 heap storage 也只在这些 gate 释放后析构。
pub(super) struct PreparedRegularWriteStaging {
    stack: [core::mem::MaybeUninit<u8>; crate::memory::PAGE_SIZE],
    heap: Vec<core::mem::MaybeUninit<u8>>,
    length: usize,
}

impl PreparedRegularWriteStaging {
    /// 在任何 regular-file gate 或 publication 前准备未初始化 bounded staging。
    ///
    /// # Parameters
    ///
    /// - `total_length`: 已检查的 syscall request 总长度。
    ///
    /// # Returns
    ///
    /// 小请求使用 stack；大请求最多使用 1 MiB heap，reserve 失败退回一页 stack。
    pub(super) fn prepare(total_length: usize) -> Self {
        let desired = total_length.min(RegularFileWrite::MAX_STAGING_BYTES);
        let mut heap = Vec::new();
        let heap_ready =
            desired > crate::memory::PAGE_SIZE && heap.try_reserve_exact(desired).is_ok();
        let length = fallible_staging_capacity(desired, crate::memory::PAGE_SIZE, heap_ready);
        if heap_ready {
            // reserve 已完成，resize 只建立已预留容量内的 initialized byte range。
            heap.resize_with(length, core::mem::MaybeUninit::uninit);
        }
        Self {
            stack: [core::mem::MaybeUninit::uninit(); crate::memory::PAGE_SIZE],
            heap,
            length,
        }
    }

    /// 借出已经分配、不会在使用期间扩容的 initialized-prefix owner。
    ///
    /// # Returns
    ///
    /// capacity 不超过 1 MiB 的 syscall-local input staging。
    pub(super) fn as_input_staging(&mut self) -> UserInputStaging<'_> {
        if self.heap.is_empty() {
            UserInputStaging::from_slice(&mut self.stack[..self.length])
        } else {
            UserInputStaging::from_slice(&mut self.heap)
        }
    }
}

/// 将一个或多个 userspace vector 作为一次 contiguous regular-file write 执行。
///
/// # Parameters
///
/// - `task`: userspace address owner 与 RLIMIT_FSIZE/SIGXFSZ source。
/// - `file`: 持有整次 syscall write-sequence ownership 的 regular-file mutation facade。
/// - `position`: 本次操作唯一 byte offset；append 时投影实际 inode-end placement。
/// - `vectors`: 按序消费的 userspace buffers。
/// - `append`: 本次 operation 是否按 O_APPEND/RWF_APPEND 选择 inode end；若逐 chunk 重读 flags，
///   并发 F_SETFL 会把一次 syscall 分裂为 append 与 positioned 两种语义。
/// - `staging`: 在 position/write-sequence gate 外完成 allocation 的 initialized-prefix owner。
///
/// # Returns
///
/// 总写入字节数、首错负 errno 或已有进度后的 partial count。
pub(super) fn write_vectors(
    task: &TaskControlBlock,
    file: &RegularFileWrite<'_>,
    position: &mut u64,
    vectors: &[UserIoVec],
    append: bool,
    staging: &mut UserInputStaging<'_>,
) -> isize {
    let total_length = vectors
        .iter()
        .try_fold(0usize, |total, vector| total.checked_add(vector.length))
        .expect("regular write vectors must have a checked total length");
    if total_length == 0 {
        return 0;
    }
    assert!(
        staging.capacity() != 0,
        "non-empty regular write requires prepared staging"
    );
    let mut cursor = UserIoCursor::new(vectors);
    let mut total = 0usize;
    while total < total_length {
        let chunk_length = regular_write_chunk(total_length, total, staging.capacity());
        assert_ne!(chunk_length, 0, "regular write staging made no progress");
        // 1. non-append range 在 copyin 前应用同一 syscall 的累计 limit accounting。
        let effective_position = if append { file.size() } else { *position };
        let requested = match bounded_regular_write(task, effective_position, chunk_length, total) {
            Ok(count) => count,
            Err(result) => return result,
        };
        if let Err(error) = file.prepare_content_write() {
            return if total == 0 {
                ferr(error)
            } else {
                total as isize
            };
        }
        let staged = cursor.stage_from_user_pagewise_into(task, staging, requested);
        if staged.count == 0 {
            return if staged.faulted && total == 0 {
                -errno::EFAULT
            } else {
                total as isize
            };
        }
        // 2. append 的 size selection 与 storage mutation 仍由 page-cache operation owner 原子提交。
        let written = if append {
            match file.append(staging.initialized(), task.file_size_limit()) {
                Ok((_, 0)) => {
                    return if total == 0 {
                        file_size_exceeded(task)
                    } else {
                        total as isize
                    };
                }
                Ok((offset, written)) => {
                    *position = offset
                        .checked_add(written as u64)
                        .expect("storage append returned an overflowing byte range");
                    written
                }
                Err(error) => {
                    return if total == 0 {
                        ferr(error)
                    } else {
                        total as isize
                    };
                }
            }
        } else {
            match file.write(*position, staging.initialized()) {
                Ok(written) => {
                    *position += written as u64;
                    written
                }
                Err(error) => {
                    return if total == 0 {
                        ferr(error)
                    } else {
                        total as isize
                    };
                }
            }
        };
        cursor.advance(written);
        total += written;
        task.account_write_storage(written);
        // 3. faulted prefix 与 storage short 都只发布实际 backend progress；不越过 staged suffix。
        if staged.faulted || written < staged.count {
            return total as isize;
        }
    }
    total as isize
}
