use alloc::sync::Arc;
use core::error::Error;

use crate::memory::page_table::PageTableError;

/// @description 为 memory transaction 构造可失败的共享 owner。
/// @param value 尚未发布、失败时可直接析构的 owner value。
/// @return Arc control block 分配成功时返回 owner；失败返回统一 OutOfMemory。
pub(super) fn try_memory_arc<T>(value: T) -> Result<Arc<T>, MemoryError> {
    Arc::try_new(value).map_err(|_| MemoryError::OutOfMemory)
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum MemoryError {
    OutOfMemory,
    PageTableError(PageTableError),
    InvalidRange,
    AddressInUse,
    PermissionDenied,
    Io,
}

impl From<PageTableError> for MemoryError {
    fn from(error: PageTableError) -> Self {
        Self::PageTableError(error)
    }
}

impl core::fmt::Display for MemoryError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutOfMemory => write!(formatter, "Out of memory"),
            Self::PageTableError(error) => write!(formatter, "Page table error: {error}"),
            Self::InvalidRange => write!(formatter, "Invalid virtual memory range"),
            Self::AddressInUse => write!(formatter, "Virtual memory range is already mapped"),
            Self::PermissionDenied => write!(formatter, "Virtual memory operation is not allowed"),
            Self::Io => write!(formatter, "File-backed memory I/O failed"),
        }
    }
}

impl Error for MemoryError {}

impl MemoryError {
    /// @description 判断失败是否来自物理页或页表页资源耗尽，不向上层泄漏页表错误类型。
    ///
    /// @return 资源耗尽返回 true，其他地址或权限错误返回 false。
    pub(crate) fn is_out_of_memory(self) -> bool {
        matches!(
            self,
            Self::OutOfMemory
                | Self::PageTableError(
                    PageTableError::OutOfMemory | PageTableError::AddressSpaceIdentifiersExhausted
                )
        )
    }
}

/// @description 用户地址复制失败原因；所有成员都表示不能完成完整 copyin/copyout。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UserAccessError {
    /// 地址为空、非用户 canonical 地址、未映射或权限不匹配。
    Fault,
    /// 地址加长度发生整数溢出。
    Overflow,
    /// 在调用方指定上限内没有找到 NUL。
    Unterminated,
    /// 无法为 kernel-owned copy 缓冲区分配内存。
    OutOfMemory,
}

impl core::fmt::Display for UserAccessError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Fault => write!(formatter, "invalid user address or permission"),
            Self::Overflow => write!(formatter, "user address range overflow"),
            Self::Unterminated => write!(formatter, "unterminated user string"),
            Self::OutOfMemory => write!(formatter, "out of memory while copying user string"),
        }
    }
}

impl Error for UserAccessError {}

/// @description 构造新用户映像时需要暴露给 `execve` 的失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElfLoadError {
    /// 物理页或页表页分配失败。
    OutOfMemory,
    /// ELF header、segment、地址、权限、解释器或初始栈不满足当前架构契约。
    InvalidElf,
    /// executable source 在 transaction 构造期间发生 I/O error 或 short read。
    Io,
}

impl From<MemoryError> for ElfLoadError {
    fn from(error: MemoryError) -> Self {
        match error {
            MemoryError::OutOfMemory
            | MemoryError::PageTableError(
                PageTableError::OutOfMemory | PageTableError::AddressSpaceIdentifiersExhausted,
            ) => Self::OutOfMemory,
            MemoryError::PageTableError(_)
            | MemoryError::InvalidRange
            | MemoryError::AddressInUse
            | MemoryError::PermissionDenied => Self::InvalidElf,
            MemoryError::Io => Self::Io,
        }
    }
}

impl core::fmt::Display for ElfLoadError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutOfMemory => write!(formatter, "out of memory while loading ELF"),
            Self::InvalidElf => write!(formatter, "invalid or unsupported architecture ELF image"),
            Self::Io => write!(formatter, "I/O error while loading ELF"),
        }
    }
}

impl Error for ElfLoadError {}
