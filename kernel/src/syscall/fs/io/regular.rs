use super::*;

/// @description 将一个或多个 userspace vector 作为一次 contiguous regular-file read 执行。
/// @param task userspace address owner。
/// @param file 已解析 page-cache identity 的 regular file。
/// @param position 本次操作唯一 byte offset；仅在成功 copyout 后推进。
/// @param vectors 按序消费的 userspace buffers。
/// @return 总读取字节数、EOF 零、首错负 errno 或已有进度后的 partial count。
pub(super) fn read_vectors(
    task: &TaskControlBlock,
    file: &RegularFile,
    position: &mut u64,
    vectors: &[UserIoVec],
) -> isize {
    let mut total = 0usize;
    // PAGE_SIZE 同时匹配 page-cache frame 与 user-copy fault/copy 粒度；保持 512-byte staging
    // 会让满页顺序 I/O 重复八次 facade dispatch、地址计算和 page walk。
    let mut chunk = [0u8; crate::memory::PAGE_SIZE];
    for vector in vectors {
        let mut done = 0usize;
        while done < vector.length {
            let count = chunk.len().min(vector.length - done);
            // 1. 每个 chunk 复用同一 file/position；vector 之间不重新解析 fd 或 cache identity。
            let read = match file.read(*position, &mut chunk[..count]) {
                Ok(read) => read,
                Err(error) => {
                    return if total == 0 {
                        ferr(error)
                    } else {
                        total as isize
                    };
                }
            };
            task.account_read_storage(read.storage_bytes);
            let read = read.bytes;
            if read == 0 {
                return total as isize;
            }
            // 2. offset 只在完整 copyout 后提交；坏 user range 不得消费未交付的文件 bytes。
            let Some(address) = vector.base.checked_add(done) else {
                return if total == 0 {
                    -errno::EFAULT
                } else {
                    total as isize
                };
            };
            if task.copy_to_user(address, &chunk[..read]).is_err() {
                return if total == 0 {
                    -errno::EFAULT
                } else {
                    total as isize
                };
            }
            *position += read as u64;
            done += read;
            total += read;
            // 3. regular cache 仅会在 EOF 产生 short read；不得跳到后续 vector 越过 EOF。
            if read < count {
                return total as isize;
            }
        }
    }
    total as isize
}

/// regular write 的 syscall-local transient staging owner。
///
/// `prepare` 必须在 OFD position/write-sequence gate 外调用，`with_prepared_staging`
/// 则保证 heap storage 也只在这些 gate 释放后析构。
pub(super) struct PreparedRegularWriteStaging {
    stack: [u8; crate::memory::PAGE_SIZE],
    heap: Vec<u8>,
    length: usize,
}

impl PreparedRegularWriteStaging {
    /// @description 在任何 regular-file gate 或 publication 前准备并清零 bounded staging。
    /// @param total_length 已检查的 syscall request 总长度。
    /// @return 小请求使用 stack；大请求最多使用 128 KiB heap，reserve 失败退回一页 stack。
    pub(super) fn prepare(total_length: usize) -> Self {
        let desired = total_length.min(RegularFileWrite::MAX_STAGING_BYTES);
        let mut heap = Vec::new();
        let heap_ready =
            desired > crate::memory::PAGE_SIZE && heap.try_reserve_exact(desired).is_ok();
        let length = fallible_staging_capacity(desired, crate::memory::PAGE_SIZE, heap_ready);
        if heap_ready {
            // reserve 已完成，resize 只建立已预留容量内的 initialized byte range。
            heap.resize(length, 0);
        }
        Self {
            stack: [0u8; crate::memory::PAGE_SIZE],
            heap,
            length,
        }
    }

    /// @description 借出已经分配、清零且不会在使用期间扩容的 staging slice。
    /// @return 长度不超过 128 KiB 的 syscall-local buffer。
    pub(super) fn as_mut_slice(&mut self) -> &mut [u8] {
        if self.heap.is_empty() {
            &mut self.stack[..self.length]
        } else {
            &mut self.heap
        }
    }
}

/// @description 将一个或多个 userspace vector 作为一次 contiguous regular-file write 执行。
/// @param task userspace address owner 与 RLIMIT_FSIZE/SIGXFSZ source。
/// @param file 持有整次 syscall write-sequence ownership 的 regular-file mutation facade。
/// @param position 本次操作唯一 byte offset；append 时投影实际 inode-end placement。
/// @param vectors 按序消费的 userspace buffers。
/// @param append 本次 operation 是否按 O_APPEND/RWF_APPEND 选择 inode end；若逐 chunk 重读 flags，
/// 并发 F_SETFL 会把一次 syscall 分裂为 append 与 positioned 两种语义。
/// @param staging 在 position/write-sequence gate 外完成 allocation/zero-fill 的固定长度 slice。
/// @return 总写入字节数、首错负 errno 或已有进度后的 partial count。
pub(super) fn write_vectors(
    task: &TaskControlBlock,
    file: &RegularFileWrite<'_>,
    position: &mut u64,
    vectors: &[UserIoVec],
    append: bool,
    staging: &mut [u8],
) -> isize {
    let total_length = vectors
        .iter()
        .try_fold(0usize, |total, vector| total.checked_add(vector.length))
        .expect("regular write vectors must have a checked total length");
    if total_length == 0 {
        return 0;
    }
    assert!(
        !staging.is_empty(),
        "non-empty regular write requires prepared staging"
    );
    let chunk = staging;
    let mut cursor = UserIoCursor::new(vectors);
    let mut total = 0usize;
    while total < total_length {
        let chunk_length = regular_write_chunk(total_length, total, chunk.len());
        assert_ne!(chunk_length, 0, "regular write staging made no progress");
        // 1. non-append range 在 copyin 前应用同一 syscall 的累计 limit accounting。
        let requested = if append {
            chunk_length
        } else {
            match bounded_regular_write(task, *position, chunk_length, total) {
                Ok(count) => count,
                Err(result) => return result,
            }
        };
        let staged = cursor.stage_from_user_pagewise(task, &mut chunk[..requested]);
        if staged.count == 0 {
            return if staged.faulted && total == 0 {
                -errno::EFAULT
            } else {
                total as isize
            };
        }
        // 2. append 的 size selection 与 storage mutation 仍由 page-cache operation owner 原子提交。
        let written = if append {
            match file.append(&chunk[..staged.count], task.file_size_limit()) {
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
            match file.write(*position, &chunk[..staged.count]) {
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
