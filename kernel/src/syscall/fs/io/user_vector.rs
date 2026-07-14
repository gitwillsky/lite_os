use super::*;

/// Linux limits one readv/writev transaction to 1024 iovec entries.
pub(super) const IOV_MAX: usize = 1024;

/// Linux RV64 userspace `struct iovec` layout.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct UserIoVec {
    /// Userspace buffer 起始地址。
    pub(super) base: usize,
    /// Userspace buffer byte count。
    pub(super) length: usize,
}

const USER_IO_VEC_SIZE: usize = mem::size_of::<UserIoVec>();

/// @description 一次性导入并验证 Linux RV64 iovec 数组，供 sequential/positioned I/O 共用。
/// @param task userspace address owner。
/// @param iovector userspace iovec 数组地址；count 为零时可为空。
/// @param count iovec 数量，最大 IOV_MAX。
/// @return 已导入 vector 与总长度。
/// @error count 超限或总长度超过 SSIZE_MAX 时返回 `EINVAL`。
/// @error iovec 数组地址无效或溢出时返回 `EFAULT`。
/// @error kernel vector 分配失败时返回 `ENOMEM`。
pub(super) fn import_iovecs(
    task: &TaskControlBlock,
    iovector: usize,
    count: usize,
) -> Result<(Vec<UserIoVec>, usize), isize> {
    // 1. 先拒绝不可能合法的 array shape，避免分配和 userspace walk。
    if count > IOV_MAX {
        return Err(-errno::EINVAL);
    }
    if count != 0 && iovector == 0 {
        return Err(-errno::EFAULT);
    }
    let mut vectors = Vec::new();
    vectors
        .try_reserve_exact(count)
        .map_err(|_| -errno::ENOMEM)?;
    let mut total_length = 0usize;
    let mut imported = 0usize;
    // 与 user-copy 的页粒度一致；逐 iovec copy 会让合法的最大数组重复进入 user-copy 1024 次。
    let mut bytes = [0u8; crate::memory::PAGE_SIZE];
    let vectors_per_chunk = bytes.len() / mem::size_of::<UserIoVec>();
    while imported < count {
        // 2. 连续 iovec array 按 userspace 页批量 copyin；跨页 entry 保持单独导入，避免较晚
        // 页 fault 抢在较早 entry 的非法 length 前返回，改变现有错误优先级。
        let address = imported
            .checked_mul(USER_IO_VEC_SIZE)
            .and_then(|offset| iovector.checked_add(offset))
            .ok_or(-errno::EFAULT)?;
        let bytes_to_page_end = crate::memory::PAGE_SIZE - address % crate::memory::PAGE_SIZE;
        let chunk_count = if bytes_to_page_end < USER_IO_VEC_SIZE {
            1
        } else {
            (count - imported)
                .min(vectors_per_chunk)
                .min(bytes_to_page_end / USER_IO_VEC_SIZE)
        };
        let byte_count = chunk_count * USER_IO_VEC_SIZE;
        task.copy_from_user(address, &mut bytes[..byte_count])
            .map_err(|_| -errno::EFAULT)?;
        for bytes in bytes[..byte_count].as_chunks::<USER_IO_VEC_SIZE>().0 {
            let vector = UserIoVec {
                base: usize::from_ne_bytes(bytes[..mem::size_of::<usize>()].try_into().unwrap()),
                length: usize::from_ne_bytes(bytes[mem::size_of::<usize>()..].try_into().unwrap()),
            };
            // 3. SSIZE_MAX 是一次 vector syscall 可报告 byte count 的硬上限。
            total_length = total_length
                .checked_add(vector.length)
                .filter(|length| *length <= isize::MAX as usize)
                .ok_or(-errno::EINVAL)?;
            vectors.push(vector);
        }
        imported += chunk_count;
    }
    Ok((vectors, total_length))
}

/// @description 一次 non-regular sequential I/O 内唯一的 userspace scatter/gather progress owner。
///
/// OWNER: cursor 唯一维护 vector index、vector 内 offset 与 completed byte count；若 caller
/// 复制这些字段并人工同步，EFAULT 后会重复消费或跳过 userspace bytes，破坏 partial count。
pub(super) struct UserIoCursor<'a> {
    vectors: &'a [UserIoVec],
    index: usize,
    offset: usize,
    completed: usize,
}

impl<'a> UserIoCursor<'a> {
    /// @description 从第一个非空 vector 创建未消费 cursor。
    /// @param vectors scalar one-element 或已导入且总长度不超过 SSIZE_MAX 的 vectors。
    /// @return completed 为零的新 cursor。
    pub(super) fn new(vectors: &'a [UserIoVec]) -> Self {
        Self {
            vectors,
            index: 0,
            offset: 0,
            completed: 0,
        }
    }

    /// @description 返回已成功 copyin/copyout 的总字节数。
    /// @return 跨所有已消费 vectors 的 monotonic byte count。
    pub(super) fn completed(&self) -> usize {
        self.completed
    }

    /// @description 从 cursor 指向的 userspace vectors gather 到连续 kernel buffer。
    /// @param task userspace address owner。
    /// @param output kernel-owned destination；最多复制其长度。
    /// @return 实际复制字节数，vectors 耗尽时可短于 output。
    /// @error 当前 user range 地址溢出或不可读时返回 unit；cursor 保留此前成功进度。
    pub(super) fn copy_from_user(
        &mut self,
        task: &TaskControlBlock,
        output: &mut [u8],
    ) -> Result<usize, ()> {
        let mut copied = 0usize;
        while copied < output.len() && self.index < self.vectors.len() {
            // 1. 空 vector 与刚好耗尽的 vector 不产生 userspace access。
            let vector = self.vectors[self.index];
            if self.offset == vector.length {
                self.index += 1;
                self.offset = 0;
                continue;
            }
            let count = (vector.length - self.offset).min(output.len() - copied);
            let address = vector.base.checked_add(self.offset).ok_or(())?;
            // 2. 只提交当前 vector 内的连续 range，避免跨独立 mapping 合并验证。
            task.copy_from_user(address, &mut output[copied..copied + count])
                .map_err(|_| ())?;
            // 3. copy 成功后再推进唯一 progress；失败时 caller 可返回精确 partial count。
            self.offset += count;
            self.completed += count;
            copied += count;
        }
        Ok(copied)
    }

    /// @description 将连续 kernel bytes scatter 到 cursor 指向的 userspace vectors。
    /// @param task userspace address owner。
    /// @param input kernel-owned source；最多复制其长度。
    /// @return 实际复制字节数，vectors 耗尽时可短于 input。
    /// @error 当前 user range 地址溢出或不可写时返回 unit；cursor 保留此前成功进度。
    pub(super) fn copy_to_user(
        &mut self,
        task: &TaskControlBlock,
        input: &[u8],
    ) -> Result<usize, ()> {
        let mut copied = 0usize;
        while copied < input.len() && self.index < self.vectors.len() {
            // 1. 空 vector 与刚好耗尽的 vector 不产生 userspace access。
            let vector = self.vectors[self.index];
            if self.offset == vector.length {
                self.index += 1;
                self.offset = 0;
                continue;
            }
            let count = (vector.length - self.offset).min(input.len() - copied);
            let address = vector.base.checked_add(self.offset).ok_or(())?;
            // 2. 只提交当前 vector 内的连续 range，避免跨独立 mapping 合并验证。
            task.copy_to_user(address, &input[copied..copied + count])
                .map_err(|_| ())?;
            // 3. copy 成功后再推进唯一 progress；失败时 caller 可返回精确 partial count。
            self.offset += count;
            self.completed += count;
            copied += count;
        }
        Ok(copied)
    }

    /// @description 在不推进 cursor 的前提下证明后续 prefix 可写。
    /// @param task userspace address owner。
    /// @param length 必须覆盖的 byte count。
    /// @return 全部 prefix 已 fault-in 且可写。
    /// @error vectors 不足、地址溢出或任一 range 不可写时返回 unit。
    pub(super) fn validate_write_prefix(
        &self,
        task: &TaskControlBlock,
        mut length: usize,
    ) -> Result<(), ()> {
        let mut index = self.index;
        let mut offset = self.offset;
        // 1. 使用 shadow progress，保证 validation 不改变真实 cursor。
        while length != 0 && index < self.vectors.len() {
            let vector = self.vectors[index];
            if offset == vector.length {
                index += 1;
                offset = 0;
                continue;
            }
            let count = (vector.length - offset).min(length);
            let address = vector.base.checked_add(offset).ok_or(())?;
            // 2. 每个 vector 独立 fault-in，保留 iovec mapping 边界。
            task.validate_user_write(address, count).map_err(|_| ())?;
            offset += count;
            length -= count;
        }
        // 3. vectors 提前耗尽必须失败，否则 destructive read 会消费无法 copyout 的 value。
        (length == 0).then_some(()).ok_or(())
    }
}
