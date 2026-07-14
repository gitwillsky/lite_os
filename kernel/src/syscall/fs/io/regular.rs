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

/// @description 将一个或多个 userspace vector 作为一次 contiguous regular-file write 执行。
/// @param task userspace address owner 与 RLIMIT_FSIZE/SIGXFSZ source。
/// @param file 持有整次 syscall write-sequence ownership 的 regular-file mutation facade。
/// @param position 本次操作唯一 byte offset；append 时投影实际 inode-end placement。
/// @param vectors 按序消费的 userspace buffers。
/// @param append 本次 operation 是否按 O_APPEND/RWF_APPEND 选择 inode end；若逐 chunk 重读 flags，
/// 并发 F_SETFL 会把一次 syscall 分裂为 append 与 positioned 两种语义。
/// @return 总写入字节数、首错负 errno 或已有进度后的 partial count。
pub(super) fn write_vectors(
    task: &TaskControlBlock,
    file: &RegularFileWrite<'_>,
    position: &mut u64,
    vectors: &[UserIoVec],
    append: bool,
) -> isize {
    let mut total = 0usize;
    // 与 read path 共用 page-sized staging，保证 scalar/vector/positioned write 具有
    // 相同的 bounded stack 成本，不按入口复制不同 chunk policy。
    let mut chunk = [0u8; crate::memory::PAGE_SIZE];
    for vector in vectors {
        let mut done = 0usize;
        while done < vector.length {
            let requested = chunk.len().min(vector.length - done);
            // 1. non-append range 在 copyin 前应用同一 syscall 的累计 limit accounting。
            let count = if append {
                requested
            } else {
                match bounded_regular_write(task, *position, requested, total) {
                    Ok(count) => count,
                    Err(result) => return result,
                }
            };
            let Some(address) = vector.base.checked_add(done) else {
                return if total == 0 {
                    -errno::EFAULT
                } else {
                    total as isize
                };
            };
            if task.copy_from_user(address, &mut chunk[..count]).is_err() {
                return if total == 0 {
                    -errno::EFAULT
                } else {
                    total as isize
                };
            }
            // 2. append 的 size selection 与 storage mutation 仍由 page-cache operation owner 原子提交。
            let written = if append {
                match file.append(&chunk[..count], task.file_size_limit()) {
                    Ok((_, 0)) if count != 0 => {
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
                match file.write(*position, &chunk[..count]) {
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
            done += written;
            total += written;
            task.account_write_storage(written);
            // 3. storage short write 终止完整 vector operation，禁止跳到下一段制造非连续结果。
            if written < count {
                return total as isize;
            }
        }
    }
    total as isize
}
