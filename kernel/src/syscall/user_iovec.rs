use alloc::vec::Vec;
use core::mem::{self, MaybeUninit};

#[path = "user_iovec/input_staging.rs"]
mod input_staging;
#[allow(unused_imports)]
pub(super) use input_staging::UserInputStaging;

#[cfg(not(test))]
use crate::task::TaskControlBlock;

/// Linux limits one vector I/O operation to 1024 iovec entries.
pub(super) const IOV_MAX: usize = 1024;

/// Linux RV64 userspace `struct iovec` layout.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct UserIoVec {
    /// Userspace buffer 起始地址。
    pub(super) base: usize,
    /// Userspace buffer byte count。
    pub(super) length: usize,
}

const USER_IO_VEC_SIZE: usize = mem::size_of::<UserIoVec>();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ImportError {
    TooMany,
    NullArray,
    AddressOverflow,
    CopyFault,
    NoMemory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BufferError {
    NullBase,
    AddressOverflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TotalLengthError {
    Overflow,
    Limit,
}

fn checked_iovec_address(base: usize, index: usize) -> Option<usize> {
    index
        .checked_mul(USER_IO_VEC_SIZE)
        .and_then(|offset| base.checked_add(offset))
}

/// @description 从唯一 raw RV64 iovec ABI 布局按 userspace page 批量导入 entries。
/// @param iovector userspace iovec array address；count 为零时允许为零。
/// @param count entry count，最大 IOV_MAX。
/// @param copy 执行一次连续 userspace copyin 的 adapter。
/// @return 保持 userspace 顺序的 raw entries；不解释 subsystem length policy。
pub(super) fn import_iovecs_with(
    iovector: usize,
    count: usize,
    mut copy: impl FnMut(usize, &mut [u8]) -> Result<(), ()>,
) -> Result<Vec<UserIoVec>, ImportError> {
    if count > IOV_MAX {
        return Err(ImportError::TooMany);
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    if iovector == 0 {
        return Err(ImportError::NullArray);
    }
    // Half-open raw array range 必须可表示；只验证首 entry address 会遗漏尾端 wrap。
    checked_iovec_address(iovector, count).ok_or(ImportError::AddressOverflow)?;

    let mut vectors = Vec::new();
    vectors
        .try_reserve_exact(count)
        .map_err(|_| ImportError::NoMemory)?;
    let mut imported = 0usize;
    let mut bytes = [0u8; crate::memory::PAGE_SIZE];
    let vectors_per_chunk = bytes.len() / USER_IO_VEC_SIZE;
    while imported < count {
        let address =
            checked_iovec_address(iovector, imported).ok_or(ImportError::AddressOverflow)?;
        let bytes_to_page_end = crate::memory::PAGE_SIZE - address % crate::memory::PAGE_SIZE;
        // Entry 跨页时单独 copy，避免后页 fault 越过该 entry 改变错误顺序；其余 entries
        // 只在当前 userspace page 内聚合，最大数组由四次左右 copyin 完成。
        let chunk_count = if bytes_to_page_end < USER_IO_VEC_SIZE {
            1
        } else {
            (count - imported)
                .min(vectors_per_chunk)
                .min(bytes_to_page_end / USER_IO_VEC_SIZE)
        };
        let byte_count = chunk_count * USER_IO_VEC_SIZE;
        copy(address, &mut bytes[..byte_count]).map_err(|_| ImportError::CopyFault)?;
        for bytes in bytes[..byte_count].as_chunks::<USER_IO_VEC_SIZE>().0 {
            vectors.push(UserIoVec {
                base: usize::from_ne_bytes(bytes[..mem::size_of::<usize>()].try_into().unwrap()),
                length: usize::from_ne_bytes(bytes[mem::size_of::<usize>()..].try_into().unwrap()),
            });
        }
        imported += chunk_count;
    }
    Ok(vectors)
}

/// @description 生产 user-copy adapter；raw importer 不拥有 errno policy。
#[cfg(not(test))]
pub(super) fn import_iovecs(
    task: &TaskControlBlock,
    iovector: usize,
    count: usize,
) -> Result<Vec<UserIoVec>, ImportError> {
    import_iovecs_with(iovector, count, |address, output| {
        task.copy_from_user(address, output).map_err(|_| ())
    })
}

/// @description 验证每个非空 userspace buffer 的 half-open address range。
pub(super) fn validate_user_buffers(vectors: &[UserIoVec]) -> Result<(), BufferError> {
    for vector in vectors {
        if vector.length == 0 {
            continue;
        }
        if vector.base == 0 {
            return Err(BufferError::NullBase);
        }
        vector
            .base
            .checked_add(vector.length)
            .ok_or(BufferError::AddressOverflow)?;
    }
    Ok(())
}

/// @description 按 caller 选择的 maximum 计算 checked vector total。
pub(super) fn checked_total_length(
    vectors: &[UserIoVec],
    maximum: usize,
) -> Result<usize, TotalLengthError> {
    let mut total = 0usize;
    for vector in vectors {
        total = total
            .checked_add(vector.length)
            .ok_or(TotalLengthError::Overflow)?;
        if total > maximum {
            return Err(TotalLengthError::Limit);
        }
    }
    Ok(total)
}

/// @description 按 caller 选择的上限截断 vector suffix，并返回可传输 prefix 总长。
/// @param vectors 保持 entry 顺序；越过 maximum 的 entry length 被截断，后续置零。
/// @return 不超过 maximum 的有效总长；不拥有任何 subsystem errno policy。
pub(super) fn project_total_length(vectors: &mut [UserIoVec], maximum: usize) -> usize {
    let mut total = 0usize;
    for vector in vectors {
        let length = vector.length.min(maximum - total);
        vector.length = length;
        total += length;
    }
    total
}

/// @description 返回 remaining request 的下一段固定上限 staging capacity。
pub(super) fn bounded_staging_capacity(remaining: usize, maximum: usize) -> usize {
    remaining.min(maximum)
}

/// @description 在 stack fast path 与可选 heap staging 之间选择实际容量。
/// @param desired 已由 subsystem maximum 限制的期望 byte 数。
/// @param stack_capacity allocation-free fallback 容量。
/// @param heap_ready 大 staging 的 reserve 是否已在 publication 前成功。
/// @return 小请求保持精确容量；大请求仅在 reserve 成功时扩大，否则退回 stack。
pub(super) fn fallible_staging_capacity(
    desired: usize,
    stack_capacity: usize,
    heap_ready: bool,
) -> usize {
    if desired <= stack_capacity || heap_ready {
        desired
    } else {
        stack_capacity
    }
}

/// @description 在 operation callback 外拥有并最终释放已准备好的 transient staging。
/// @param prepared callback 开始前已完成 storage 分配的 staging owner。
/// @param operation 可包含 OFD position/write-sequence gate；只能借用 prepared owner。
/// @return operation 的原样结果；staging 在 callback 返回、相关 gate 释放后才析构。
/// @note 这里只保证 staging reserve/drop 不与 gate 重叠；user fault 与 backend
/// transaction 仍可按各自契约分配。若把 staging 生命周期操作移入 callback，allocator/reclaimer
/// 可能在 filesystem spin lock 内重入。
pub(super) fn with_prepared_staging<Staging, Output>(
    mut prepared: Staging,
    operation: impl FnOnce(&mut Staging) -> Output,
) -> Output {
    let output = operation(&mut prepared);
    drop(prepared);
    output
}

/// 一次只读 stage 的结果；`count` 是否可提交由具体 staging seam 定义。
///
/// `stage_with`/socket 在 fault 时丢弃 whole-stage，`stage_pagewise_with`/regular write
/// 则允许提交 fault 前的 `count` prefix。
pub(super) struct StagedCopy {
    pub(super) count: usize,
    pub(super) faulted: bool,
}

/// @description 一次 scatter/gather I/O 内唯一的 userspace progress owner。
pub(super) struct UserIoCursor<'a> {
    vectors: &'a [UserIoVec],
    index: usize,
    offset: usize,
    completed: usize,
}

impl<'a> UserIoCursor<'a> {
    pub(super) fn new(vectors: &'a [UserIoVec]) -> Self {
        Self {
            vectors,
            index: 0,
            offset: 0,
            completed: 0,
        }
    }

    pub(super) fn completed(&self) -> usize {
        self.completed
    }

    /// @description 不推进 progress 地 gather prefix；caller 只 commit backend 已消费 bytes。
    pub(super) fn stage_with(
        &self,
        output: &mut [u8],
        mut copy: impl FnMut(usize, &mut [u8]) -> Result<(), ()>,
    ) -> StagedCopy {
        let mut index = self.index;
        let mut offset = self.offset;
        let mut copied = 0usize;
        while copied < output.len() && index < self.vectors.len() {
            let vector = self.vectors[index];
            if offset == vector.length {
                index += 1;
                offset = 0;
                continue;
            }
            let count = (vector.length - offset).min(output.len() - copied);
            let Some(address) = vector.base.checked_add(offset) else {
                return StagedCopy {
                    count: copied,
                    faulted: true,
                };
            };
            if copy(address, &mut output[copied..copied + count]).is_err() {
                return StagedCopy {
                    count: copied,
                    faulted: true,
                };
            }
            offset += count;
            copied += count;
        }
        StagedCopy {
            count: copied,
            faulted: false,
        }
    }

    fn stage_uninit_with(
        &self,
        output: &mut [MaybeUninit<u8>],
        mut copy: impl FnMut(usize, &mut [MaybeUninit<u8>]) -> Result<(), ()>,
    ) -> StagedCopy {
        let mut index = self.index;
        let mut offset = self.offset;
        let mut copied = 0usize;
        while copied < output.len() && index < self.vectors.len() {
            let vector = self.vectors[index];
            if offset == vector.length {
                index += 1;
                offset = 0;
                continue;
            }
            let count = (vector.length - offset).min(output.len() - copied);
            let Some(address) = vector.base.checked_add(offset) else {
                return StagedCopy {
                    count: copied,
                    faulted: true,
                };
            };
            if copy(address, &mut output[copied..copied + count]).is_err() {
                return StagedCopy {
                    count: copied,
                    faulted: true,
                };
            }
            offset += count;
            copied += count;
        }
        StagedCopy {
            count: copied,
            faulted: false,
        }
    }

    pub(super) fn stage_uninit_pagewise_with(
        &self,
        output: &mut [MaybeUninit<u8>],
        mut copy: impl FnMut(usize, &mut [MaybeUninit<u8>]) -> Result<(), ()>,
    ) -> StagedCopy {
        let mut index = self.index;
        let mut offset = self.offset;
        let mut copied = 0usize;
        while copied < output.len() && index < self.vectors.len() {
            let vector = self.vectors[index];
            if offset == vector.length {
                index += 1;
                offset = 0;
                continue;
            }
            let Some(address) = vector.base.checked_add(offset) else {
                return StagedCopy {
                    count: copied,
                    faulted: true,
                };
            };
            let to_page_end = crate::memory::PAGE_SIZE - address % crate::memory::PAGE_SIZE;
            let count = (vector.length - offset)
                .min(output.len() - copied)
                .min(to_page_end);
            if copy(address, &mut output[copied..copied + count]).is_err() {
                return StagedCopy {
                    count: copied,
                    faulted: true,
                };
            }
            offset += count;
            copied += count;
        }
        StagedCopy {
            count: copied,
            faulted: false,
        }
    }

    #[cfg(not(test))]
    pub(super) fn stage_from_user(&self, task: &TaskControlBlock, output: &mut [u8]) -> StagedCopy {
        self.stage_with(output, |address, output| {
            task.copy_from_user(address, output).map_err(|_| ())
        })
    }

    #[cfg(not(test))]
    pub(super) fn stage_from_user_into(
        &self,
        task: &TaskControlBlock,
        staging: &mut UserInputStaging<'_>,
        length: usize,
    ) -> StagedCopy {
        let staged = self.stage_uninit_with(staging.prepare(length), |address, output| {
            task.copy_from_user_uninit(address, output).map_err(|_| ())
        });
        // SAFETY: stage_uninit_with 只在 copy_from_user_uninit 完整初始化一个 chunk 后计入
        // count；失败 chunk 不计入，故 staged prefix 的每个 byte 都已初始化。
        unsafe { staging.publish(staged.count) };
        staged
    }

    #[cfg(not(test))]
    pub(super) fn stage_from_user_pagewise_into(
        &self,
        task: &TaskControlBlock,
        staging: &mut UserInputStaging<'_>,
        length: usize,
    ) -> StagedCopy {
        let staged = self.stage_uninit_pagewise_with(staging.prepare(length), |address, output| {
            task.copy_from_user_uninit(address, output).map_err(|_| ())
        });
        // SAFETY: pagewise cursor 只在 copy_from_user_uninit 完整初始化一个 page chunk 后
        // 计入 count；失败 chunk 不计入，故 staged prefix 已完整初始化。
        unsafe { staging.publish(staged.count) };
        staged
    }

    /// @description 只提交 backend 已消费的 staged prefix，避免 short send 跳过 suffix。
    pub(super) fn advance(&mut self, mut count: usize) {
        while count != 0 {
            let vector = self.vectors[self.index];
            if self.offset == vector.length {
                self.index += 1;
                self.offset = 0;
                continue;
            }
            let advanced = count.min(vector.length - self.offset);
            self.offset += advanced;
            self.completed += advanced;
            count -= advanced;
        }
    }

    /// @description 按 iovec range 清零，并仅提交已经成功完成的 vector progress。
    /// @param zero 接收 checked address 与该 vector 的剩余长度；一次调用覆盖完整连续 range。
    /// @return 本次成功清零的总字节数；失败时保留先前 vector 的 partial progress。
    pub(super) fn zero_with(
        &mut self,
        mut zero: impl FnMut(usize, usize) -> Result<(), ()>,
    ) -> Result<usize, ()> {
        let initial = self.completed;
        while self.index < self.vectors.len() {
            let vector = self.vectors[self.index];
            if self.offset == vector.length {
                self.index += 1;
                self.offset = 0;
                continue;
            }
            let count = vector.length - self.offset;
            let address = vector.base.checked_add(self.offset).ok_or(())?;
            zero(address, count)?;
            self.offset += count;
            self.completed += count;
        }
        Ok(self.completed - initial)
    }

    #[cfg(not(test))]
    pub(super) fn zero_to_user(&mut self, task: &TaskControlBlock) -> Result<usize, ()> {
        self.zero_with(|address, count| task.zero_user(address, count).map_err(|_| ()))
    }

    #[cfg(not(test))]
    pub(super) fn copy_from_user(
        &mut self,
        task: &TaskControlBlock,
        output: &mut [u8],
    ) -> Result<usize, ()> {
        let staged = self.stage_from_user(task, output);
        self.advance(staged.count);
        if staged.faulted {
            Err(())
        } else {
            Ok(staged.count)
        }
    }

    #[cfg(not(test))]
    pub(super) fn copy_from_user_into(
        &mut self,
        task: &TaskControlBlock,
        staging: &mut UserInputStaging<'_>,
        length: usize,
    ) -> Result<usize, ()> {
        let staged = self.stage_from_user_into(task, staging, length);
        self.advance(staged.count);
        if staged.faulted {
            Err(())
        } else {
            Ok(staged.count)
        }
    }

    #[cfg(not(test))]
    pub(super) fn copy_to_user(
        &mut self,
        task: &TaskControlBlock,
        input: &[u8],
    ) -> Result<usize, ()> {
        let mut copied = 0usize;
        while copied < input.len() && self.index < self.vectors.len() {
            let vector = self.vectors[self.index];
            if self.offset == vector.length {
                self.index += 1;
                self.offset = 0;
                continue;
            }
            let count = (vector.length - self.offset).min(input.len() - copied);
            let address = vector.base.checked_add(self.offset).ok_or(())?;
            task.copy_to_user(address, &input[copied..copied + count])
                .map_err(|_| ())?;
            self.offset += count;
            self.completed += count;
            copied += count;
        }
        Ok(copied)
    }

    #[cfg(not(test))]
    pub(super) fn validate_write_prefix(
        &self,
        task: &TaskControlBlock,
        mut length: usize,
    ) -> Result<(), ()> {
        let mut index = self.index;
        let mut offset = self.offset;
        while length != 0 && index < self.vectors.len() {
            let vector = self.vectors[index];
            if offset == vector.length {
                index += 1;
                offset = 0;
                continue;
            }
            let count = (vector.length - offset).min(length);
            let address = vector.base.checked_add(offset).ok_or(())?;
            task.validate_user_write(address, count).map_err(|_| ())?;
            offset += count;
            length -= count;
        }
        (length == 0).then_some(()).ok_or(())
    }
}
