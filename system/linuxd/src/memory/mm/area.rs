//! 一个 VMA：区间、当前权限、后备对象与 Linux 标志。

use alloc::sync::Arc;

use native_runtime::Vmar;
use native_runtime::native_abi::vm;

use super::request::{DeviceMappingSource, FileMappingSource};
use crate::memory::{MapPermission, MemoryError, MemoryObject, SharedMemoryMembership};

bitflags::bitflags! {
    /// VMA 的 Linux 标志。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub(crate) struct AreaFlags: u16 {
        /// `MAP_GROWSDOWN`：紧邻下方的缺页扩展该区。
        const GROWS_DOWN = 1 << 0;
        /// `MADV_DONTFORK`。
        const DONT_FORK = 1 << 1;
        /// `MADV_WIPEONFORK`：子进程得到零内容。
        const WIPE_ON_FORK = 1 << 2;
        /// `MADV_DONTDUMP`。
        const DONT_DUMP = 1 << 3;
    }
}

/// 匿名私有区的用途（`/proc/<pid>/maps` 的路径列）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnonymousRole {
    Plain,
    Heap,
    Stack,
}

/// VMA 的后备。`offset` 是区间起点在后备对象中的字节偏移；拆分时右半部分的偏移随之前移。
#[derive(Debug, Clone)]
pub(crate) enum Backing {
    /// 私有匿名：对象只属于本地址空间。
    Anonymous {
        object: Arc<MemoryObject>,
        offset: u64,
        role: AnonymousRole,
    },
    /// 共享匿名：fork 后父子映射同一对象。
    SharedAnonymous { object: Arc<MemoryObject>, offset: u64 },
    /// 文件私有映射：对象是页缓存的写时复制子对象，`file_offset` 是区间起点对应的文件偏移。
    PrivateFile {
        source: FileMappingSource,
        object: Arc<MemoryObject>,
        offset: u64,
        file_offset: u64,
    },
    /// 文件共享映射：直接映射页缓存对象，`offset` 即文件偏移。
    SharedFile {
        source: FileMappingSource,
        object: Arc<MemoryObject>,
        offset: u64,
        writable: bool,
    },
    /// 设备缓冲。
    Device { source: DeviceMappingSource, offset: u64 },
    /// SysV 共享内存段。
    SystemV { source: SharedMemoryMembership, offset: u64 },
    /// 信号返回跳板（全部地址空间共享的只读可执行页）。
    Trampoline { object: Arc<MemoryObject> },
}

impl Backing {
    /// 后备对象。
    pub(crate) fn object(&self) -> &Arc<MemoryObject> {
        match self {
            Self::Anonymous { object, .. }
            | Self::SharedAnonymous { object, .. }
            | Self::PrivateFile { object, .. }
            | Self::SharedFile { object, .. }
            | Self::Trampoline { object } => object,
            Self::Device { source, .. } => source.backing.object(),
            Self::SystemV { source, .. } => source.source.backing.object(),
        }
    }

    /// 区间起点在后备对象中的偏移。
    pub(crate) fn offset(&self) -> u64 {
        match self {
            Self::Anonymous { offset, .. }
            | Self::SharedAnonymous { offset, .. }
            | Self::PrivateFile { offset, .. }
            | Self::SharedFile { offset, .. }
            | Self::Device { offset, .. }
            | Self::SystemV { offset, .. } => *offset,
            Self::Trampoline { .. } => 0,
        }
    }

    /// 起点前移 `delta` 字节后的同一后备（拆分右半部分）。
    pub(crate) fn advanced(&self, delta: u64) -> Self {
        let mut backing = self.clone();
        match &mut backing {
            Self::Anonymous { offset, .. }
            | Self::SharedAnonymous { offset, .. }
            | Self::Device { offset, .. }
            | Self::SystemV { offset, .. } => *offset += delta,
            Self::PrivateFile { offset, file_offset, .. } => {
                *offset += delta;
                *file_offset += delta;
            }
            Self::SharedFile { offset, .. } => *offset += delta,
            Self::Trampoline { .. } => {}
        }
        backing
    }

    /// 映射对其他地址空间可见（写入对共享者可见、futex 使用共享 key）。
    pub(crate) fn is_shared(&self) -> bool {
        matches!(
            self,
            Self::SharedAnonymous { .. } | Self::SharedFile { .. } | Self::Device { .. } | Self::SystemV { .. }
        )
    }

    /// 允许的最大权限（共享文件只读打开时不能加写）。
    pub(crate) fn may(&self, permission: MapPermission) -> bool {
        match self {
            Self::SharedFile { writable, .. } => *writable || !permission.contains(MapPermission::W),
            Self::Trampoline { .. } => !permission.contains(MapPermission::W),
            Self::SystemV { source, .. } => source.source.may_write || !permission.contains(MapPermission::W),
            _ => true,
        }
    }
}

/// 一个 VMA。
#[derive(Debug, Clone)]
pub(crate) struct Area {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) permission: MapPermission,
    pub(crate) backing: Backing,
    pub(crate) flags: AreaFlags,
    /// `PR_SET_VMA_ANON_NAME`。
    pub(crate) name: Option<Arc<[u8]>>,
}

impl Area {
    pub(crate) fn len(&self) -> usize {
        self.end - self.start
    }

    /// 在 `address` 处切分，返回 `[address, end)` 部分；本区保留 `[start, address)`。
    pub(crate) fn split_off(&mut self, address: usize) -> Self {
        debug_assert!(self.start < address && address < self.end);
        let right = Self {
            start: address,
            end: self.end,
            permission: self.permission,
            backing: self.backing.advanced((address - self.start) as u64),
            flags: self.flags,
            name: self.name.clone(),
        };
        self.end = address;
        right
    }

    /// 把本区安装到 `vmar`（覆盖区间内已有映射）。
    ///
    /// # Errors
    ///
    /// 内核拒绝映射（权限超过句柄上限、内存不足）。
    pub(crate) fn install(&self, vmar: &Vmar, base: usize) -> Result<(), MemoryError> {
        let object = self.backing.object();
        vmar.map(
            vm::SPECIFIC_OVERWRITE | self.permission.vm_options(),
            self.start - base,
            object.vmo(),
            self.backing.offset(),
            self.len(),
        )
        .map(|_| ())
        .map_err(MemoryError::from_status)
    }

    /// 本区中的数据（可写私有）页数贡献给 `RLIMIT_DATA`。
    pub(crate) fn counts_as_data(&self) -> bool {
        self.permission.contains(MapPermission::W) && !self.backing.is_shared()
            && !matches!(self.backing, Backing::Anonymous { role: AnonymousRole::Stack, .. })
    }
}
