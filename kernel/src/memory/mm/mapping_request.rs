use alloc::sync::Arc;

use crate::memory::{FrameTracker, SharedFileMapping};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryAdvice {
    Normal,
    Random,
    Sequential,
    WillNeed,
    DontNeed,
    Free,
}

/// @description file-backed VMA 的稳定 backing 与 page-aligned 文件偏移。
pub(crate) struct FileMappingSource {
    pub(super) mapping: Arc<dyn SharedFileMapping>,
    pub(super) offset: u64,
}

/// @description device-backed mmap 在 DRM 与 memory seam 之间传递的不可变 backing view。
#[derive(Debug, Clone)]
pub(crate) struct DeviceMappingSource {
    pub(super) identity: u64,
    pub(super) backing: Arc<FrameTracker>,
    pub(super) page_offset: usize,
}

impl DeviceMappingSource {
    /// @description 构造从 backing 首页开始的 device mapping source。
    ///
    /// @param identity 在 backing 释放后仍不复用的共享 futex identity。
    /// @param backing 完整物理 extent 的共享生命周期 owner。
    /// @return page offset 为零的 mapping source。
    pub(crate) fn new(identity: u64, backing: Arc<FrameTracker>) -> Self {
        Self {
            identity,
            backing,
            page_offset: 0,
        }
    }
}

impl FileMappingSource {
    /// @description 组合 filesystem mapping adapter 与对应起始偏移。
    ///
    /// @param mapping regular-file page-cache adapter。
    /// @param offset page-aligned 文件起始偏移，由 memory owner 最终校验。
    /// @return 单次 VMA transaction 消费的 file source。
    pub(crate) fn new(mapping: Arc<dyn SharedFileMapping>, offset: u64) -> Self {
        Self { mapping, offset }
    }
}

/// @description 新建 private mapping 同时消费的 `RLIMIT_AS/RLIMIT_DATA` 快照。
#[derive(Debug, Clone, Copy)]
pub(crate) struct MappingResourceLimits {
    pub(super) address_space: u64,
    pub(super) data: u64,
}

impl MappingResourceLimits {
    /// @description 组合一次 mapping transaction 的两项 Process 资源边界。
    ///
    /// @param address_space 用户 VMA 总字节上限。
    /// @param data writable private data 总字节上限。
    /// @return 不可变限制快照。
    pub(crate) const fn new(address_space: u64, data: u64) -> Self {
        Self {
            address_space,
            data,
        }
    }
}
