//! Resource 的不可变授权范围；父子只在创建时校验，同类子范围不能扩大或升级为其他种类。
use super::super::{KernelObject, next_koid};
use core::ops::Range;
use native_abi::{ObjectType, Status, rights};

/// resource 授权的范围。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResourceKind {
    Root,
    Mmio(Range<u64>),
    Irq(Range<u32>),
    System(Range<u64>),
}

/// 不可变的硬件/系统操作授权；对象 kind 与范围是唯一 authority，不复制服务角色。
pub(crate) struct Resource {
    koid: u64,
    kind: ResourceKind,
}

impl Resource {
    /// 根 resource；内核启动时创建一次交给根服务。
    pub(crate) fn root() -> Self {
        Self {
            koid: next_koid(),
            kind: ResourceKind::Root,
        }
    }

    /// 是否为根 resource。
    pub(crate) fn is_root(&self) -> bool {
        self.kind == ResourceKind::Root
    }

    /// 切出子 resource。
    ///
    /// # Errors
    /// 超出父范围、区间为空或 MMIO 覆盖 RAM 时返回 `ERR_INVALID_ARGS`/`ERR_ACCESS_DENIED`。
    pub(crate) fn create_child(&self, kind: u32, base: u64, size: u64) -> Result<Self, Status> {
        let end = base
            .checked_add(size)
            .filter(|_| size != 0)
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        let kind = match kind {
            native_abi::resource::MMIO => {
                if !base.is_multiple_of(native_abi::PAGE_SIZE as u64)
                    || !size.is_multiple_of(native_abi::PAGE_SIZE as u64)
                {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                let ram = crate::platform::physical_memory_range();
                if base < ram.end as u64 && end > ram.start as u64 {
                    return Err(native_abi::ERR_ACCESS_DENIED);
                }
                ResourceKind::Mmio(base..end)
            }
            native_abi::resource::IRQ => {
                let start = u32::try_from(base).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                let end = u32::try_from(end).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                ResourceKind::Irq(start..end)
            }
            native_abi::resource::SYSTEM => {
                if end > native_abi::resource::system::COUNT {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                ResourceKind::System(base..end)
            }
            _ => return Err(native_abi::ERR_INVALID_ARGS),
        };
        let allowed = match (&self.kind, &kind) {
            (ResourceKind::Root, _) => true,
            (ResourceKind::Mmio(parent), ResourceKind::Mmio(child)) => {
                parent.start <= child.start && child.end <= parent.end
            }
            (ResourceKind::Irq(parent), ResourceKind::Irq(child)) => {
                parent.start <= child.start && child.end <= parent.end
            }
            (ResourceKind::System(parent), ResourceKind::System(child)) => {
                parent.start <= child.start && child.end <= parent.end
            }
            _ => false,
        };
        if !allowed {
            return Err(native_abi::ERR_ACCESS_DENIED);
        }
        Ok(Self {
            koid: next_koid(),
            kind,
        })
    }

    /// 是否授权指定 system operation；根是全部 resource 的授权来源，其他 kind 不能跨类升级。
    /// 参数为 native resource::system 的 operation 编号，返回其是否包含在本对象的 immutable 范围内。
    pub(crate) fn covers_system(&self, operation: u64) -> bool {
        operation < native_abi::resource::system::COUNT
            && match &self.kind {
                ResourceKind::Root => true,
                ResourceKind::System(range) => range.contains(&operation),
                ResourceKind::Mmio(_) | ResourceKind::Irq(_) => false,
            }
    }

    /// 是否授权 MMIO 区间。
    pub(crate) fn covers_mmio(&self, base: u64, size: u64) -> bool {
        let Some(end) = base.checked_add(size) else {
            return false;
        };
        match &self.kind {
            ResourceKind::Root => {
                let ram = crate::platform::physical_memory_range();
                !(base < ram.end as u64 && end > ram.start as u64)
            }
            ResourceKind::Mmio(range) => range.start <= base && end <= range.end,
            ResourceKind::Irq(_) | ResourceKind::System(_) => false,
        }
    }

    /// 是否授权中断向量。
    pub(crate) fn covers_irq(&self, vector: u32) -> bool {
        match &self.kind {
            ResourceKind::Root => crate::platform::device_interrupt_range().contains(&vector),
            ResourceKind::Irq(range) => range.contains(&vector),
            ResourceKind::Mmio(_) | ResourceKind::System(_) => false,
        }
    }
}

impl KernelObject for Resource {
    fn object_type(&self) -> ObjectType {
        ObjectType::Resource
    }
    fn koid(&self) -> u64 {
        self.koid
    }
    fn default_rights(&self) -> u32 {
        rights::DUPLICATE | rights::TRANSFER | rights::INSPECT | rights::WRITE
    }
}
