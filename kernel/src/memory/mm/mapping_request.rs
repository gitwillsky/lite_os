use alloc::sync::Arc;

use crate::memory::{DeviceBacking, SharedFileMapping};

use super::{FilePageRange, FilePageRangeError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryAdvice {
    Normal,
    Random,
    Sequential,
    WillNeed,
    DontNeed,
    Free,
}

/// file-backed VMA 的稳定 backing 与 page-aligned 文件偏移。
#[derive(Clone)]
pub(crate) struct FileMappingSource {
    pub(super) mapping: Arc<dyn SharedFileMapping>,
    pub(super) pages: FilePageRange,
}

/// regular-file mmap source 在发布前的 ABI 范围校验结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileMappingError {
    Invalid,
    Overflow,
}

/// device-backed mmap 在 DRM 与 memory seam 之间传递的不可变 backing view。
#[derive(Debug, Clone)]
pub(crate) struct DeviceMappingSource {
    pub(super) identity: u64,
    pub(super) backing: Arc<DeviceBacking>,
    pub(super) page_offset: usize,
    pub(super) owner: Option<Arc<dyn crate::memory::MappedFileOwner>>,
}

impl DeviceMappingSource {
    /// 构造从 backing 首页开始的 device mapping source。
    ///
    /// # Parameters
    ///
    /// - `identity`: 在 backing 释放后仍不复用的共享 futex identity。
    /// - `backing`: 完整 scatter/gather 物理页集合的共享生命周期 owner。
    ///
    /// # Returns
    ///
    /// page offset 为零的 mapping source。
    pub(crate) fn new(identity: u64, backing: Arc<DeviceBacking>) -> Self {
        Self {
            identity,
            backing,
            page_offset: 0,
            owner: None,
        }
    }

    /// mmap ABI 已获得实际 OFD；kernel-only backing template 无需制造 dummy owner。
    pub(crate) fn retain_file(&mut self, owner: Arc<dyn crate::memory::MappedFileOwner>) {
        assert!(self.owner.is_none(), "device mapping file owner replaced");
        self.owner = Some(owner);
    }
}

impl FileMappingSource {
    /// 组合 filesystem mapping adapter 与对应起始偏移。
    ///
    /// # Parameters
    ///
    /// - `mapping`: regular-file page-cache adapter。
    /// - `offset`: regular-file mmap 的页对齐文件起始偏移。
    /// - `length`: 原始非零 mmap 字节长度。
    ///
    /// # Returns
    ///
    /// 已按 Linux signed file ceiling 验证的 file source。
    pub(crate) fn new(
        mapping: Arc<dyn SharedFileMapping>,
        offset: u64,
        length: usize,
    ) -> Result<Self, FileMappingError> {
        let pages = FilePageRange::new(offset, length).map_err(|error| match error {
            FilePageRangeError::Invalid => FileMappingError::Invalid,
            FilePageRangeError::Overflow => FileMappingError::Overflow,
        })?;
        Ok(Self { mapping, pages })
    }
}

/// 新建 private mapping 同时消费的 `RLIMIT_AS/RLIMIT_DATA` 快照。
#[derive(Debug, Clone, Copy)]
pub(crate) struct MappingResourceLimits {
    pub(super) address_space: u64,
    pub(super) data: u64,
}

impl MappingResourceLimits {
    /// 组合一次 mapping transaction 的两项 Process 资源边界。
    ///
    /// # Parameters
    ///
    /// - `address_space`: 用户 VMA 总字节上限。
    /// - `data`: writable private data 总字节上限。
    ///
    /// # Returns
    ///
    /// 不可变限制快照。
    pub(crate) const fn new(address_space: u64, data: u64) -> Self {
        Self {
            address_space,
            data,
        }
    }
}

/// mmap/shmat 的地址选择语义；raw Linux flags 只存在于 syscall codec。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MappingPlacement {
    Hint,
    Fixed,
    FixedNoReplace,
}
/// 已授权的实际 mapping backing 输入；clone 只保活准备资源，不产生 VMA membership。
#[derive(Clone)]
pub(crate) enum UserMappingSource {
    PrivateAnonymous,
    SharedAnonymous,
    PrivateFile(FileMappingSource),
    SharedFile(FileMappingSource),
    Device(DeviceMappingSource),
    SystemV(crate::memory::SharedMemoryMappingSource),
}
/// 一次 user mapping transaction 的完整不可变输入。
pub(crate) struct UserMappingRequest {
    pub(crate) address: usize,
    pub(crate) length: usize,
    pub(crate) permission: crate::memory::MapPermission,
    pub(crate) placement: MappingPlacement,
    pub(crate) source: UserMappingSource,
}
