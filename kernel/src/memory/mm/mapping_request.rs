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

/// 设备 mmap 在 Linux mm counter 中的记账方式；由设备按其对应 Linux 驱动的 mmap 路径声明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DevicePageAccounting {
    /// `VM_PFNMAP` 特殊 PTE（DRM GEM shmem 的 `vmf_insert_pfn`）：不计入 statm resident/shared。
    PfnMap,
    /// `vm_insert_page(s)` 插入的普通页（ALSA vmalloc ring、io_uring region）：计入 `MM_FILEPAGES`。
    InsertedPages,
}

/// device-backed mmap 在 DRM 与 memory seam 之间传递的不可变 backing view。
#[derive(Debug, Clone)]
pub(crate) struct DeviceMappingSource {
    pub(super) identity: u64,
    pub(super) backing: Arc<DeviceBacking>,
    pub(super) page_offset: usize,
    pub(super) owner: Option<Arc<dyn crate::memory::MappedFileOwner>>,
    pub(super) accounting: DevicePageAccounting,
}

/// 一份 VMA lease 同时保活实际 OFD 与独立设备对象；只在两者不同的映射准备阶段分配。
/// split/fork 只 clone 外层 Arc，退休完成后一起释放，避免扩大每个 MapArea 或在 unmap 时分配。
#[derive(Debug)]
struct DeviceMappingLease {
    _file: Arc<dyn crate::memory::MappedFileOwner>,
    _object: Arc<dyn crate::memory::MappedFileOwner>,
}
impl crate::memory::MappedFileOwner for DeviceMappingLease {}

impl DeviceMappingSource {
    /// 构造从 backing 首页开始的 device mapping source。
    ///
    /// # Parameters
    ///
    /// - `identity`: 在 backing 释放后仍不复用的共享 futex identity。
    /// - `backing`: 完整 scatter/gather 物理页集合的共享生命周期 owner。
    /// - `accounting`: 对应 Linux mmap 路径的 RSS 记账方式。
    ///
    /// # Returns
    ///
    /// page offset 为零的 mapping source。
    pub(crate) fn new(
        identity: u64,
        backing: Arc<DeviceBacking>,
        accounting: DevicePageAccounting,
    ) -> Self {
        Self {
            identity,
            backing,
            page_offset: 0,
            owner: None,
            accounting,
        }
    }

    /// mmap ABI 安装实际 OFD；若已有独立对象 owner，则组合成同一份 VMA lease。
    ///
    /// # Parameters
    ///
    /// - `file`: 实际 mmap 描述符的 OFD；调用者只能在 source 发布前安装一次。
    ///
    /// # Errors
    ///
    /// 组合 lease 分配失败返回 AllocError；原 source 不变，尚未修改页表。
    pub(crate) fn retain_file(
        &mut self,
        file: Arc<dyn crate::memory::MappedFileOwner>,
    ) -> Result<(), alloc::alloc::AllocError> {
        self.owner = Some(match &self.owner {
            Some(object) => Arc::try_new(DeviceMappingLease {
                _file: file,
                _object: object.clone(),
            })?,
            None => file,
        });
        Ok(())
    }

    /// 保活映射的设备对象；GEM_CLOSE 可撤销 handle，但不能提前销毁仍有 VMA 的宿主资源。
    ///
    /// # Parameters
    ///
    /// - `object`: 独立于实际 OFD 的 backend 对象 owner；split/fork 与 TLB 退休共用此引用。
    ///
    /// # Panics
    ///
    /// 同一 source 重复安装对象 owner 表示设备 adapter 违反唯一所有权契约。
    pub(crate) fn retain_object(&mut self, object: Arc<dyn crate::memory::MappedFileOwner>) {
        assert!(self.owner.is_none(), "device mapping object owner replaced");
        self.owner = Some(object);
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
