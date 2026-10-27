mod area;
mod area_publication;
mod background_reclaim;
mod cow;
mod device_area;
mod error;
mod executable_load;
mod fault_preflight;
mod file_page_range;
mod futex_atomic;
mod futex_key;
mod initial_stack;
mod mapping_request;
mod mmap;
mod private_area;
mod private_fault;
mod private_reclaim;
mod process;
mod resident;
mod retire;
use retire::PreparedAreaRetirement;
mod shared_area;
mod shared_swap;
pub(super) mod shootdown;
mod statistics;
pub(crate) use statistics::UserPageStatistics;
mod swap;
mod thread_context;
mod user_access;
mod vma_index_state;
use super::config;
use super::permissions::MapPermission;
use super::retire::{PrivateReclaimWalk, reclaim_release_decision, revoke_and_synchronize};
use super::{address::VirtualPageNumber, page_table::PageTable};
use crate::fallible_tree::{FallibleMap, VacantEntry};
use crate::memory::{
    ReclaimRequest, ReclaimResult, SharedFileError, SharedFileId, SharedFileMapping, SharedPage,
    address::{PhysicalAddress, PhysicalPageNumber, VirtualAddress},
    frame_allocator::{FrameTracker, alloc},
    page_table::{PagePermissions, PageTableEntry},
    strampoline,
};
use alloc::{sync::Arc, vec::Vec};
use area::VmaKind;
pub(crate) use area::{MapArea, MapType};
pub(crate) use background_reclaim::reclaim_address_space;
use core::ops::Range;
use device_area::DeviceArea;
use error::try_memory_arc;
use fault_preflight::{
    FaultPermissions, FaultPreflight, FaultResidency, FileFaultState, preflight_fault,
};
use file_page_range::{FilePageRange, FilePageRangeError};
pub(crate) use mmap::{RemapError, RemapPlacement, RemapRequest};
use private_area::{PrivateFaultPreparation, PrivateFileArea};
use resident::{PrivateBacking, PrivateResident};
pub(crate) use shared_area::AnonymousSharedBacking as SharedMemoryBacking;
use shared_area::{AnonymousSharedBacking, SharedAnonymousArea, SharedFileArea, SharedResident};
use shootdown::{
    TranslationCommit, TranslationSynchronizationError, revoke_and_commit,
    synchronize_address_space_retirement,
};
use vma_index_state::{VmaContribution, VmaIndexState};
pub(crate) use {
    error::{ElfLoadError, MemoryError, UserAccessError},
    fault_preflight::{FaultAccess as PageFaultAccess, SegmentationCause},
    futex_atomic::{UserAtomicU32, UserAtomicU32Op},
    futex_key::{FutexAccessError, FutexKey},
    initial_stack::{InitialStack, InitialStackCredentials},
    mapping_request::{
        DeviceMappingSource, FileMappingError, FileMappingSource, MappingPlacement,
        MappingResourceLimits, MemoryAdvice, UserMappingRequest, UserMappingSource,
    },
    mmap::PageFaultOutcome,
    user_access::UserFaultLimits,
};
/// Linux `mm_struct` 中 program break 的唯一进程级元数据。
#[derive(Debug, Clone, Copy)]
struct ProgramBreak {
    /// ELF writable image 之后允许的最小 break。
    base: usize,
    /// 用户可观察的精确 byte address；不能用页对齐 VMA end 替代。
    current: usize,
    /// 与用户 stack guard 隔离的最大 break。
    limit: usize,
}

#[derive(Debug)]
pub(crate) struct MemorySet {
    page_table: PageTable,
    areas: FallibleMap<VirtualPageNumber, MapArea>,
    // OWNER: main executable open survives munmap; last mm releases actual deny-write after translation fence.
    executable_owner: Option<Arc<dyn super::MappedFileOwner>>,
    // OWNER: 与 areas structural publication 同一事务维护；缺失会让每次 fault/mmap 为
    // RLIMIT totals 全表扫描。stack identity 直接由有序 VMA 查询，structural mutation 必须经
    // commit_area/take_area_entry/remove_area，禁止旁路更新或从表重算第二份真相。
    vma_index_state: VmaIndexState,
    // OWNER: cursor 与 VMA residency 由同一 MemorySet lock 拥有，只表示下次
    // private direct reclaim 的起始 VPN。缺失它会让大 mm 中的 dirty/
    // non-discardable 前缀在每次 OOM 时重复被扫描，饿死后续 clean page。
    private_reclaim_cursor: VirtualPageNumber,
    // OWNER: context placement hint 只决定搜索起点；每次仍验证实际 areas，不复制占用状态。
    // 成功 publication 才推进，temporary context retirement 回指刚释放页，防止 lifetime TID 耗尽 VA。
    thread_context_hint: Option<usize>,
    // OWNER: 完整 user-copy preparation 的临时保护区间；自身 reclaim 必须跳过它，
    // 否则后页 fault 可撤销已准备的前页，使有效 buffer 错误 EFAULT。仅在同一 mm 独占
    // borrow 内设置/恢复，不拥有第二份 residency，不跨 task 阻塞之外的操作传播。
    private_reclaim_exclusion: Option<Range<VirtualPageNumber>>,
    /// OWNER: Linux mm 的主 ELF start_code/end_code；若从 VMA 权限反推，JIT 映射会被误计为 statm text。
    code_range: Range<usize>,
    /// OWNER: program break 与 VMA 表同属 `MemorySet`；实际 heap 页必须是普通 anonymous
    /// VMA，否则 `MAP_FIXED` 无法按 Linux 语义替换其中任意页。
    program_break: Option<ProgramBreak>,
    // OWNER: Linux mm 的 arg_start/arg_end；缺失时 procfs 只能伪造静态 argv，无法反映用户栈修改。
    argument_range: Range<usize>,
}

impl MemorySet {
    /// 投影 Linux find_vma 的 existence：包含 address 或位于其后的首个用户 VMA。
    ///
    /// 仅用于坏 signal frame 的冷错误路径；不 fault-in、不改变 VMA/PTE。
    pub(crate) fn has_user_vma_at_or_after(&self, address: usize) -> bool {
        if address >= config::USER_ADDRESS_END {
            return false;
        }
        let vpn = VirtualAddress::from(address).floor();
        if let Some((_, area)) = self.areas.floor(&vpn)
            && area.map_permission.contains(MapPermission::U)
            && area.vpn_range.end > vpn
        {
            return true;
        }
        self.areas.iter_from(&vpn).any(|(_, area)| {
            area.map_permission.contains(MapPermission::U) && area.vpn_range.end > vpn
        })
    }
    const MMAP_BASE: usize = 0x4000_0000;
    pub(crate) fn new_kernel() -> Self {
        Self {
            page_table: PageTable::new(crate::arch::mmu::AddressSpaceKind::Kernel),
            areas: FallibleMap::new(),
            executable_owner: None,
            vma_index_state: VmaIndexState::new(),
            private_reclaim_cursor: VirtualPageNumber::from_vpn(0),
            thread_context_hint: None,
            private_reclaim_exclusion: None,
            code_range: 0..0,
            program_break: None,
            argument_range: 0..0,
        }
    }

    /// 创建未激活的空用户页表；task 在 fork 首次继承 backing 前登记完整 AddressSpace owner。
    /// root/ASID 的生命周期保持不变，任何 allocation 失败均不发布半个 owner。
    pub(crate) fn try_new() -> Result<Self, MemoryError> {
        Ok(Self {
            page_table: PageTable::try_new(crate::arch::mmu::AddressSpaceKind::User)?,
            areas: FallibleMap::new(),
            executable_owner: None,
            vma_index_state: VmaIndexState::new(),
            private_reclaim_cursor: VirtualPageNumber::from_vpn(0),
            thread_context_hint: None,
            private_reclaim_exclusion: None,
            code_range: 0..0,
            program_break: None,
            argument_range: 0..0,
        })
    }

    pub(crate) fn token(&self) -> crate::arch::mmu::AddressSpaceToken {
        self.page_table.token()
    }

    /// 主程序生命周期的实际 owner；上层可检查同一 Arc 类型，不重建 inode/path 或延长已退出 Thread 的 mm。
    pub(crate) fn executable_owner(&self) -> Option<Arc<dyn super::MappedFileOwner>> {
        self.executable_owner.clone()
    }
    pub(crate) fn kernel_trap_token(&self) -> crate::arch::mmu::KernelTrapToken {
        self.page_table.kernel_trap_token()
    }

    fn virtual_bytes(&self) -> u64 {
        self.vma_index_state.virtual_bytes()
    }

    fn data_bytes(&self) -> u64 {
        self.vma_index_state.data_bytes()
    }

    fn ensure_resource_capacity(
        &self,
        additional: u64,
        address_space_limit: u64,
        data_limit: Option<u64>,
    ) -> Result<(), MemoryError> {
        if self.virtual_bytes().saturating_add(additional) > address_space_limit
            || data_limit.is_some_and(|limit| self.data_bytes().saturating_add(additional) > limit)
        {
            return Err(MemoryError::OutOfMemory);
        }
        Ok(())
    }

    pub(crate) fn map_trampoline(&mut self) -> Result<(), MemoryError> {
        let trampoline_va = VirtualAddress::from(config::TRAMPOLINE);
        let strampoline_pa = PhysicalAddress::from(
            crate::arch::mmu::virtual_to_physical(strampoline as *const () as usize)
                .expect("kernel trampoline is outside the architecture direct map"),
        );

        let mut commit = TranslationCommit::new();
        self.page_table.map(
            trampoline_va.into(),
            strampoline_pa.into(),
            // Trampoline 在所有地址空间通用，标记为 Global，避免跨进程切换时TLB混淆
            PagePermissions::READ | PagePermissions::EXECUTE | PagePermissions::GLOBAL,
            &mut commit,
        )?;
        self.page_table.map(
            VirtualAddress::from(config::SIGNAL_TRAMPOLINE).into(),
            strampoline_pa.into(),
            PagePermissions::READ | PagePermissions::EXECUTE | PagePermissions::USER,
            &mut commit,
        )?;
        commit.finish_unpublished();
        Ok(())
    }

    pub(crate) fn active(&self) {
        self.page_table.activate_kernel();
    }

    fn translate(&self, vpn: VirtualPageNumber) -> Option<PageTableEntry> {
        self.page_table.translate(vpn)
    }

    /// 将 kernel virtual address 翻译为物理地址值，不返回底层映射引用。
    ///
    /// # Parameters
    ///
    /// - `virtual_address`: kernel 需要提交给设备的虚拟地址。
    ///
    /// # Returns
    ///
    /// leaf PTE 存在时返回包含页内偏移的物理地址，否则返回 `None`。
    pub(crate) fn translate_kernel_address(
        &self,
        virtual_address: VirtualAddress,
    ) -> Option<PhysicalAddress> {
        let pte = self.translate(virtual_address.floor())?;
        let page_address = PhysicalAddress::from(pte.ppn()).as_usize();
        page_address
            .checked_add(virtual_address.page_offset())
            .map(PhysicalAddress::from)
    }

    fn initialize_user_heap(&mut self, base: usize, limit: usize) -> Result<(), MemoryError> {
        if self.program_break.is_some() || base >= limit || !VirtualAddress::from(base).is_aligned()
        {
            return Err(MemoryError::InvalidRange);
        }
        self.program_break = Some(ProgramBreak {
            base,
            current: base,
            limit,
        });
        Ok(())
    }

    /// 查询或原子提交 program break；实际页统一表示为 anonymous VMA。
    ///
    /// # Parameters
    ///
    /// - `new_break`: 零表示查询，否则为期望的新 byte address。
    /// - `address_space_limit`: 当前进程 `RLIMIT_AS` soft limit。
    /// - `data_limit`: 当前进程 `RLIMIT_DATA` soft limit。
    ///
    /// # Returns
    ///
    /// 成功返回 break 与可选的 detached retirement；caller 在 MM lock 外消费 token。
    /// 失败保持精确 break 与全部 VMA 不变。
    pub(crate) fn set_program_break(
        &mut self,
        new_break: usize,
        address_space_limit: u64,
        data_limit: u64,
    ) -> Result<(usize, Option<PreparedAreaRetirement>), MemoryError> {
        let state = self.program_break.ok_or(MemoryError::InvalidRange)?;
        if new_break == 0 {
            return Ok((state.current, None));
        }
        if new_break < state.base || new_break > state.limit {
            return Err(MemoryError::InvalidRange);
        }
        let old_page_end = VirtualAddress::from(state.current).ceil();
        let new_page_end = VirtualAddress::from(new_break).ceil();
        let mut retirement = None;
        if new_page_end > old_page_end {
            let start = usize::from(VirtualAddress::from(old_page_end));
            let length = (new_page_end.as_usize() - old_page_end.as_usize()) * config::PAGE_SIZE;
            self.map_anonymous(
                start,
                length,
                MapPermission::R | MapPermission::W | MapPermission::U,
                true,
                address_space_limit,
                data_limit,
            )?;
        } else if new_page_end < old_page_end {
            let start = usize::from(VirtualAddress::from(new_page_end));
            let length = (old_page_end.as_usize() - new_page_end.as_usize()) * config::PAGE_SIZE;
            retirement = Some(self.prepare_user_unmap(start, length)?);
        }
        self.program_break
            .as_mut()
            .expect("validated program break state must remain installed")
            .current = new_break;
        Ok((new_break, retirement))
    }

    /// 获取给定UserContext虚拟地址的物理页号
    pub(crate) fn trap_context_ppn(&self, trap_va: usize) -> PhysicalPageNumber {
        self.page_table
            .translate(VirtualAddress::from(trap_va).into())
            .expect("UserContext VA should be mapped")
            .ppn()
    }
}

impl Drop for MemorySet {
    fn drop(&mut self) {
        // 1. 此时全部 mapping/frame 仍由 self 保活；同步清除所有 CPU translation 后
        // ASID 才可复用。后上线 CPU 在 startup 执行
        // full fence，因此从未观察过本 address space 的 CPU 不属于 retirement target。
        synchronize_address_space_retirement()
            .expect("platform TLB synchronization failed while retiring address space");
        // 2. fence completion happens-before bitmap release；字段随后才析构并释放 frames。
        self.page_table
            .release_address_space_id_after_global_fence();
    }
}

#[derive(Debug, Clone)]
struct LoadedElf {
    entry: usize,
    phdr: Option<usize>,
    phent: usize,
    phnum: usize,
    max_end: usize,
    code_range: Range<usize>,
}
