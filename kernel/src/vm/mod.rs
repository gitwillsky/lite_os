//! 虚拟内存：页表、VMO、地址空间与 VMAR、缺页、用户内存复制、kernel stack。
//!
//! # 锁序
//!
//! 1. VMO 树锁（一棵 COW 树共享一把，见 [`vmo`]）。
//! 2. 地址空间锁；共享地址空间先于其成员地址空间。
//!
//! 缺页先在地址空间锁内查到映射并释放锁，再按上述顺序解析页与安装 PTE；
//! VMO 侧的撤销（COW 写保护、decommit、resize）在持树锁时逐个取得地址空间锁。
//!
//! # 退休
//!
//! 撤销翻译后，被摘下的页框与映射对象保活到 TLB fence 完成之后才释放，
//! 否则其他 CPU 的旧翻译可能访问已被复用的物理页。

mod address_space;
mod device_space;
mod fault;
mod kernel_space;
pub(crate) mod kernel_stack;
pub(crate) mod page_table;
pub(crate) mod translation;
mod vmo;

pub(crate) use address_space::{AddressSpace, MapOptions, Placement, Vmar};
pub(crate) use device_space::DeviceAddressSpace;
pub(crate) use fault::FaultAccess;
pub(crate) use kernel_space::{KERNEL_SPACE, initialize as initialize_kernel_space};
pub(crate) use kernel_stack::KernelStack;
pub(crate) use vmo::{
    CachePolicy, ChildKind, FaultSource, PagerBinding, PagerRequest, PagerSink, PinnedRange,
    RangeOp, Vmo, VmoOptions,
};

use page_table::{PagePermissions, PageTable};
use translation::TranslationCommit;

/// 虚拟内存操作的错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VmError {
    /// 物理页、页表页或索引节点分配失败。
    NoMemory,
    /// 地址、长度、对齐或选项非法。
    InvalidArgs,
    /// 区间越过对象或 VMAR 边界。
    OutOfRange,
    /// 目标区间已被占用。
    AddressInUse,
    /// 地址没有映射。
    NotMapped,
    /// 映射存在但权限不足。
    AccessDenied,
    /// 对象状态不允许（已销毁、已固定、类型不支持）。
    BadState,
    /// 对象类型不支持该操作。
    NotSupported,
    /// pager 报告页内容不可用。
    Io,
    /// 等待被进程终止或线程 kick 打断。
    Interrupted,
}

/// 用户可映射地址区间的结束（不含）：其上保留 trampoline 与 trap context。
pub(crate) const USER_END: usize =
    if crate::arch::mmu::USER_ADDRESS_END < crate::arch::mmu::TRAP_CONTEXT_ADDRESS {
        crate::arch::mmu::USER_ADDRESS_END
    } else {
        crate::arch::mmu::TRAP_CONTEXT_ADDRESS
    };
/// 用户可映射地址区间的起点；零页永不映射，空指针访问总是缺页。
pub(crate) const USER_START: usize = crate::memory::PAGE_SIZE;

/// 把共享 trampoline 映射进一张页表；所有地址空间共用同一物理页并标记为 global。
fn map_trampoline(
    page_table: &mut PageTable,
    commit: &mut TranslationCommit,
) -> Result<(), VmError> {
    let physical =
        crate::arch::mmu::virtual_to_physical(crate::memory::strampoline as *const () as usize)
            .expect("kernel trampoline is outside the architecture direct map");
    page_table
        .map(
            crate::memory::VirtualAddress::from(crate::arch::mmu::TRAMPOLINE_ADDRESS).floor(),
            crate::memory::PhysicalAddress::from(physical).floor(),
            PagePermissions::READ | PagePermissions::EXECUTE | PagePermissions::GLOBAL,
            commit,
        )
        .map_err(|_| VmError::NoMemory)
}
