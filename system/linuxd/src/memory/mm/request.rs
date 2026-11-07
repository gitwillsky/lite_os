//! 映射请求的不可变输入：raw Linux flags 只存在于 syscall 层，这里只有已授权的语义。

use alloc::sync::Arc;

use crate::memory::{MapPermission, MappedFileOwner, MemoryError, PAGE_SIZE, SharedFileMapping};
use device_model::DeviceBacking;

/// `madvise` 的语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryAdvice {
    Normal,
    Random,
    Sequential,
    WillNeed,
    DontNeed,
    Free,
    DontFork,
    DoFork,
    WipeOnFork,
    KeepOnFork,
    DontDump,
    DoDump,
}

/// 文件映射来源：页缓存接口（它持有打开文件，映射期间保活文件）+ 页对齐的文件起始偏移。
#[derive(Clone, Debug)]
pub(crate) struct FileMappingSource {
    pub(crate) mapping: Arc<dyn SharedFileMapping>,
    pub(crate) offset: u64,
}

/// 文件映射的偏移/长度校验失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileMappingError {
    Invalid,
    Overflow,
}

impl FileMappingSource {
    /// 组合页缓存接口与文件偏移，并按 Linux 有符号 `loff_t` 上限校验范围。
    ///
    /// # Errors
    ///
    /// 偏移未对齐（`Invalid`）或 `offset + length` 超过 `i64::MAX`（`Overflow`）。
    pub(crate) fn new(
        mapping: Arc<dyn SharedFileMapping>,
        offset: u64,
        length: usize,
    ) -> Result<Self, FileMappingError> {
        if !offset.is_multiple_of(PAGE_SIZE as u64) {
            return Err(FileMappingError::Invalid);
        }
        let end = offset
            .checked_add((length as u64).next_multiple_of(PAGE_SIZE as u64))
            .ok_or(FileMappingError::Overflow)?;
        if end > i64::MAX as u64 {
            return Err(FileMappingError::Overflow);
        }
        Ok(Self { mapping, offset })
    }
}

/// 设备映射来源。
#[derive(Clone, Debug)]
pub(crate) struct DeviceMappingSource {
    pub(crate) backing: Arc<DeviceBacking>,
    /// 映射用的内存对象：与 `backing` 同一 VMO（同 koid，共享 futex key 跨映射一致）。
    pub(crate) object: Arc<crate::memory::MemoryObject>,
    pub(crate) page_offset: usize,
    /// mmap 入口提供的实际文件身份与打开租约；设备构造 backing 时尚无此身份。
    pub(crate) file: Option<Arc<dyn MappedFileOwner>>,
    /// mmap 输入的文件字节偏移；不能用 VMO 中的缓冲偏移替代（DRM 的映射偏移可能是句柄）。
    pub(crate) file_offset: u64,
    /// 设备提供者交出的缓冲寿命（例如 GEM 句柄关闭后仍保活 DMA buffer）。
    retained_buffer: Option<Arc<dyn MappedFileOwner>>,
}

impl DeviceMappingSource {
    /// 从缓冲首页开始的映射来源。
    ///
    /// # Errors
    ///
    /// 复制 VMO 句柄或分配内存对象失败。
    pub(crate) fn new(backing: Arc<DeviceBacking>) -> Result<Self, MemoryError> {
        let vmo = backing
            .vmo()
            .duplicate(native_runtime::native_abi::rights::SAME)
            .map_err(MemoryError::from_status)?;
        Ok(Self {
            object: crate::memory::MemoryObject::new(vmo)?,
            backing,
            page_offset: 0,
            file: None,
            file_offset: 0,
            retained_buffer: None,
        })
    }

    /// mmap 入口设置实际文件及打开租约；不经堆分配，不能靠遍历无类型 owner 列表猜身份。
    pub(crate) fn set_file(&mut self, file: Arc<dyn MappedFileOwner>, offset: u64) {
        self.file = Some(file);
        self.file_offset = offset;
    }

    /// 设备提供者设置唯一缓冲寿命；与 mmap 入口的文件身份独立，不产生额外的克隆分配。
    pub(crate) fn retain_buffer(&mut self, buffer: Arc<dyn MappedFileOwner>) {
        self.retained_buffer = Some(buffer);
    }
}

/// 一次映射新建时消费的 `RLIMIT_AS`/`RLIMIT_DATA` 快照。
#[derive(Debug, Clone, Copy)]
pub(crate) struct MappingResourceLimits {
    pub(crate) address_space: u64,
    pub(crate) data: u64,
}

impl MappingResourceLimits {
    pub(crate) const fn new(address_space: u64, data: u64) -> Self {
        Self {
            address_space,
            data,
        }
    }
}

/// mmap/shmat 的地址选择语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MappingPlacement {
    Hint,
    Fixed,
    FixedNoReplace,
}

/// 已授权的映射后备。
#[derive(Clone, Debug)]
pub(crate) enum UserMappingSource {
    PrivateAnonymous,
    SharedAnonymous {
        object: Arc<crate::memory::MemoryObject>,
        file: Arc<dyn MappedFileOwner>,
    },
    PrivateFile(FileMappingSource),
    SharedFile(FileMappingSource),
    Device(DeviceMappingSource),
    SystemV(crate::memory::SharedMemoryMappingSource),
}

/// 一次映射的完整输入。
pub(crate) struct UserMappingRequest {
    pub(crate) address: usize,
    pub(crate) length: usize,
    pub(crate) permission: MapPermission,
    pub(crate) placement: MappingPlacement,
    pub(crate) source: UserMappingSource,
    /// `MAP_GROWSDOWN`。
    pub(crate) grows_down: bool,
}

/// `mremap` 的目标位置策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemapPlacement {
    InPlace,
    MayMove,
    Fixed(usize),
}

/// 一次 `mremap` 的输入。
#[derive(Debug, Clone, Copy)]
pub(crate) struct RemapRequest {
    pub(crate) address: usize,
    pub(crate) old_length: usize,
    pub(crate) new_length: usize,
    pub(crate) placement: RemapPlacement,
    /// `MREMAP_DONTUNMAP`：搬迁后旧区间保留为新的空匿名映射。
    pub(crate) dont_unmap: bool,
}

/// `mremap` 失败分类。
#[derive(Debug)]
pub(crate) enum RemapError {
    /// 源区间不被单个可重映射的 VMA 完整覆盖（EFAULT）。
    Unmapped,
    /// 参数组合非法（EINVAL）。
    Invalid,
    /// 原地放不下且不允许搬迁，或超出资源上限（ENOMEM）。
    NoMemory,
    Memory(MemoryError),
}

impl From<MemoryError> for RemapError {
    fn from(error: MemoryError) -> Self {
        Self::Memory(error)
    }
}
