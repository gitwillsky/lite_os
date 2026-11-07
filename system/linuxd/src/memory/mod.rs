//! Linux 进程内存：每个 Linux 地址空间是共享进程的一个成员进程，VMA 映射为其私有区 VMAR 中的 VMO 映射。
//!
//! # 分工
//!
//! - 内核：页分配、页表、写时复制、pager 缺页与 TLB 失效（VMAR/VMO 机制）。
//! - 本模块：Linux VMA 语义（mmap/munmap/mprotect/mremap/madvise/msync/brk、fork、exec 装载）、
//!   用户内存访问、futex key 与缺页分类。
//!
//! # 后备对象
//!
//! 私有匿名、brk 与栈各有自己的匿名 VMO；共享匿名、memfd、tmpfs、SysV shm 使用对象自身的 VMO；
//! 文件映射使用 inode 页缓存 VMO（私有映射为其写时复制子 VMO）；设备映射使用设备缓冲的 VMO。
//! fork 时私有映射取 `SNAPSHOT` 子 VMO，共享映射映射同一 VMO。

mod executable;
mod executable_load;
mod futex;
mod initial_stack;
mod mapping_statistics;
mod mm;
mod object;
mod shared_file;
mod shared_memory;

use crate::sync::Once;

pub(crate) use executable::{
    ExecutableImage, ExecutableParseError, ExecutableSource, parse_interpreter_elf, parse_main_elf,
};
pub(crate) use futex::{FutexAccessError, FutexKey, UserAtomicU32, UserAtomicU32Op};
pub(crate) use initial_stack::{InitialStack, InitialStackCredentials};
pub(crate) use mapping_statistics::MappingStatistics;
pub(crate) use mm::{
    AnonymousRole, DeviceMappingSource, FileMappingError, FileMappingSource, MappedMemory,
    MappingBacking, MappingPlacement, MappingResourceLimits, MappingSnapshot, MemoryAdvice,
    MemorySet, PageFaultAccess, PageFaultOutcome, RemapError, RemapPlacement, RemapRequest,
    SegmentationCause, UserMappingRequest, UserMappingSource, UserPageStatistics,
};
pub(crate) use object::MemoryObject;
pub(crate) use shared_file::{MappedFileOwner, SharedFileError, SharedFileId, SharedFileMapping};
pub(crate) use shared_memory::{
    SharedMemoryBacking, SharedMemoryMappingSource, SharedMemoryMembership, SharedMemoryVma,
};

/// 用户页大小。
pub(crate) const PAGE_SIZE: usize = native_runtime::native_abi::PAGE_SIZE;

bitflags::bitflags! {
    /// 用户映射权限（`U` 表示用户可访问，所有用户映射都带它）。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct MapPermission: u8 {
        const R = 1 << 1;
        const W = 1 << 2;
        const X = 1 << 3;
        const U = 1 << 4;
    }
}

impl MapPermission {
    /// VMAR 映射/保护选项。
    pub(crate) fn vm_options(self) -> u32 {
        use native_runtime::native_abi::vm;
        let mut options = 0;
        if self.contains(Self::R) {
            options |= vm::READ;
        }
        if self.contains(Self::W) {
            options |= vm::WRITE;
        }
        if self.contains(Self::X) {
            options |= vm::EXECUTE;
        }
        options
    }
}

/// 内存操作失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryError {
    OutOfMemory,
    InvalidRange,
    AddressInUse,
    PermissionDenied,
    Io,
}

impl MemoryError {
    pub(crate) fn is_out_of_memory(self) -> bool {
        self == Self::OutOfMemory
    }

    /// 由内核状态码映射。
    pub(crate) fn from_status(status: native_runtime::native_abi::Status) -> Self {
        use native_runtime::native_abi as abi;
        match status {
            abi::ERR_NO_MEMORY | abi::ERR_NO_RESOURCES => Self::OutOfMemory,
            abi::ERR_ACCESS_DENIED => Self::PermissionDenied,
            abi::ERR_IO => Self::Io,
            _ => Self::InvalidRange,
        }
    }
}

impl core::fmt::Display for MemoryError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutOfMemory => write!(formatter, "Out of memory"),
            Self::InvalidRange => write!(formatter, "Invalid virtual memory range"),
            Self::AddressInUse => write!(formatter, "Virtual memory range is already mapped"),
            Self::PermissionDenied => write!(formatter, "Virtual memory operation is not allowed"),
            Self::Io => write!(formatter, "File-backed memory I/O failed"),
        }
    }
}

impl core::error::Error for MemoryError {}

impl From<SharedFileError> for MemoryError {
    fn from(error: SharedFileError) -> Self {
        match error {
            SharedFileError::OutOfMemory => Self::OutOfMemory,
            SharedFileError::Io => Self::Io,
            SharedFileError::BeyondEof => Self::InvalidRange,
        }
    }
}

/// 用户内存访问失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UserAccessError {
    Fault,
    Overflow,
    Unterminated,
    OutOfMemory,
}

impl core::fmt::Display for UserAccessError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Fault => write!(formatter, "invalid user address or permission"),
            Self::Overflow => write!(formatter, "user address range overflow"),
            Self::Unterminated => write!(formatter, "unterminated user string"),
            Self::OutOfMemory => write!(formatter, "out of memory while copying user memory"),
        }
    }
}

impl core::error::Error for UserAccessError {}

/// ELF 装载失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElfLoadError {
    OutOfMemory,
    InvalidElf,
    Io,
}

impl From<MemoryError> for ElfLoadError {
    fn from(error: MemoryError) -> Self {
        match error {
            MemoryError::OutOfMemory => Self::OutOfMemory,
            MemoryError::InvalidRange
            | MemoryError::AddressInUse
            | MemoryError::PermissionDenied => Self::InvalidElf,
            MemoryError::Io => Self::Io,
        }
    }
}

impl From<SharedFileError> for ElfLoadError {
    fn from(error: SharedFileError) -> Self {
        match error {
            SharedFileError::OutOfMemory => Self::OutOfMemory,
            SharedFileError::Io => Self::Io,
            SharedFileError::BeyondEof => Self::InvalidElf,
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

impl core::error::Error for ElfLoadError {}

/// Linux 可见的用户地址区间：成员进程私有区。所有成员布局相同，首次创建地址空间时记录。
// OWNER: 本 linuxd 实例共享的私有区几何，只在首个地址空间建立时发布一次。
static USER_RANGE: Once<core::ops::Range<usize>> = Once::new();

/// Linux 用户地址空间的结束地址（不含），即 `TASK_SIZE`。
///
/// # Panics
///
/// 尚未创建任何 Linux 地址空间时调用。
pub(crate) fn user_address_end() -> usize {
    USER_RANGE
        .get()
        .expect("user address range read before the first address space")
        .end
}

fn publish_user_range(range: core::ops::Range<usize>) {
    let published = USER_RANGE.call_once(|| range.clone());
    assert_eq!(
        *published, range,
        "member address spaces disagree on the private range"
    );
}
