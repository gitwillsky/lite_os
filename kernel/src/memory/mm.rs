mod area;
mod cow;
mod device_area;
mod error;
mod executable_load;
mod fault_preflight;
mod file_page_range;
mod futex_key;
mod initial_stack;
mod mapping_request;
mod mmap;
mod private_area;
mod process;
mod resident;
mod retire;
mod shared_area;
pub(super) mod shootdown;
mod statistics;
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
    frame_allocator::{FrameTracker, alloc, alloc_copy},
    page_table::{PagePermissions, PageTableEntry},
    strampoline,
};
use alloc::{sync::Arc, vec::Vec};
use area::VmaKind;
pub(crate) use area::{MapArea, MapType};
use core::ops::Range;
use device_area::DeviceArea;
use error::try_memory_arc;
use fault_preflight::{
    FaultPermissions, FaultPreflight, FaultResidency, FileFaultState, preflight_fault,
};
use file_page_range::{FilePageRange, FilePageRangeError};
use private_area::{PrivateFaultPreparation, PrivateFileArea};
use resident::PrivateResident;
use shared_area::{AnonymousSharedBacking, SharedAnonymousArea, SharedFileArea, SharedResident};
use shootdown::{
    TranslationCommit, TranslationSynchronizationError, revoke_and_commit,
    synchronize_address_space_retirement,
};
use vma_index_state::{VmaContribution, VmaIndexState};
pub(crate) use {
    error::{ElfLoadError, MemoryError, UserAccessError},
    fault_preflight::FaultAccess as PageFaultAccess,
    futex_key::FutexKey,
    mapping_request::{
        DeviceMappingSource, FileMappingError, FileMappingSource, MappingResourceLimits,
        MemoryAdvice,
    },
    mmap::PageFaultOutcome,
    user_access::UserFaultLimits,
};
/// @description Linux `mm_struct` 中 program break 的唯一进程级元数据。
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
    // OWNER: 与 areas structural publication 同一事务维护；缺失会让每次 fault/mmap 为
    // stack identity 和 RLIMIT totals 全表扫描。所有 structural mutation 必须经
    // commit_area/take_area_entry/remove_area，禁止旁路更新或从表重算第二份真相。
    vma_index_state: VmaIndexState,
    // OWNER: cursor 与 VMA residency 由同一 MemorySet lock 拥有，只表示下次
    // private direct reclaim 的起始 VPN。缺失它会让大 mm 中的 dirty/
    // non-discardable 前缀在每次 OOM 时重复被扫描，饿死后续 clean page。
    private_reclaim_cursor: VirtualPageNumber,
    /// OWNER: Linux mm 的主 ELF start_code/end_code；若从 VMA 权限反推，JIT 映射会被误计为 statm text。
    code_range: Range<usize>,
    /// OWNER: program break 与 VMA 表同属 `MemorySet`；实际 heap 页必须是普通 anonymous
    /// VMA，否则 `MAP_FIXED` 无法按 Linux 语义替换其中任意页。
    program_break: Option<ProgramBreak>,
    // OWNER: Linux mm 的 arg_start/arg_end；缺失时 procfs 只能伪造静态 argv，无法反映用户栈修改。
    argument_range: Range<usize>,
}

impl MemorySet {
    const MMAP_BASE: usize = 0x4000_0000;
    pub(crate) fn new_kernel() -> Self {
        Self {
            page_table: PageTable::new(crate::arch::mmu::AddressSpaceKind::Kernel),
            areas: FallibleMap::new(),
            vma_index_state: VmaIndexState::new(),
            private_reclaim_cursor: VirtualPageNumber::from_vpn(0),
            code_range: 0..0,
            program_break: None,
            argument_range: 0..0,
        }
    }

    fn try_new() -> Result<Self, MemoryError> {
        Ok(Self {
            page_table: PageTable::try_new(crate::arch::mmu::AddressSpaceKind::User)?,
            areas: FallibleMap::new(),
            vma_index_state: VmaIndexState::new(),
            private_reclaim_cursor: VirtualPageNumber::from_vpn(0),
            code_range: 0..0,
            program_break: None,
            argument_range: 0..0,
        })
    }

    pub(crate) fn push(
        &mut self,
        map_area: MapArea,
        data: Option<&[u8]>,
    ) -> Result<(), MemoryError> {
        let start = map_area.vpn_range.start;
        let end = map_area.vpn_range.end;
        if !self.range_is_free(start, end) {
            return Err(MemoryError::AddressInUse);
        }
        // VMA node 必须先于 PTE publication 分配；缺少该 preflight 会在页表已经
        // 可见后因 ordered-index node OOM 无法记录其唯一 owner。
        let mut prepared = self
            .areas
            .try_prepare_vacant(start, map_area)
            .map_err(|_| MemoryError::OutOfMemory)?;
        let mut publication = TranslationCommit::new();
        if let Err(e) = prepared
            .value_mut()
            .map(&mut self.page_table, &mut publication)
        {
            // VMA node 尚未发布，但 live shared mm 的 hardware walker 已可观察 partial PTE；
            // rollback 仍须保留 prepared area owner 直到所有 CPU 完成 fence。
            let prepared = revoke_and_commit(prepared, |prepared, rollback| {
                prepared.value_mut().unmap(&mut self.page_table, rollback);
            })
            .expect("platform TLB synchronization failed during VMA map rollback");
            drop(prepared);
            return Err(e);
        }
        if let Some(data) = data
            && let Err(error) = prepared.value_mut().copy_data(data)
        {
            let prepared = revoke_and_commit(prepared, |prepared, rollback| {
                prepared.value_mut().unmap(&mut self.page_table, rollback);
            })
            .expect("platform TLB synchronization failed during VMA data rollback");
            drop(prepared);
            return Err(error);
        }
        publication
            .synchronize()
            .expect("local translation fence failed during VMA publication");
        self.commit_area(prepared);
        Ok(())
    }

    pub(crate) fn insert_framed_area(
        &mut self,
        start_va: VirtualAddress,
        end_va: VirtualAddress,
        permission: MapPermission,
    ) -> Result<(), MemoryError> {
        self.push(
            MapArea::new(start_va, end_va, MapType::Framed, permission),
            None,
        )
    }

    pub(crate) fn token(&self) -> crate::arch::mmu::AddressSpaceToken {
        self.page_table.token()
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

    fn account_area(&mut self, area: &MapArea) {
        self.vma_index_state.publish(area.index_contribution());
    }

    fn unaccount_area(&mut self, area: &MapArea) {
        self.vma_index_state.retire(area.index_contribution());
    }

    fn commit_area(&mut self, entry: VacantEntry<VirtualPageNumber, MapArea>) {
        self.account_area(entry.value());
        self.areas.commit_vacant(entry);
    }

    fn take_area_entry(
        &mut self,
        key: &VirtualPageNumber,
    ) -> Option<VacantEntry<VirtualPageNumber, MapArea>> {
        let entry = self.areas.take_entry(key)?;
        self.unaccount_area(entry.value());
        Some(entry)
    }

    fn remove_area(&mut self, key: &VirtualPageNumber) -> Option<MapArea> {
        self.take_area_entry(key).map(VacantEntry::into_value)
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

    /// @description 将 kernel virtual address 翻译为物理地址值，不返回底层映射引用。
    ///
    /// @param virtual_address kernel 需要提交给设备的虚拟地址。
    /// @return leaf PTE 存在时返回包含页内偏移的物理地址，否则返回 `None`。
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

    /// @description 查询或原子提交 program break；实际页统一表示为 anonymous VMA。
    ///
    /// @param new_break 零表示查询，否则为期望的新 byte address。
    /// @param address_space_limit 当前进程 `RLIMIT_AS` soft limit。
    /// @param data_limit 当前进程 `RLIMIT_DATA` soft limit。
    /// @return 成功返回当前或已提交的新 break；失败保持精确 break 与全部 VMA 不变。
    pub(crate) fn set_program_break(
        &mut self,
        new_break: usize,
        address_space_limit: u64,
        data_limit: u64,
    ) -> Result<usize, MemoryError> {
        let state = self.program_break.ok_or(MemoryError::InvalidRange)?;
        if new_break == 0 {
            return Ok(state.current);
        }
        if new_break < state.base || new_break > state.limit {
            return Err(MemoryError::InvalidRange);
        }
        let old_page_end = VirtualAddress::from(state.current).ceil();
        let new_page_end = VirtualAddress::from(new_break).ceil();
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
            self.unmap_user_mapping(start, length)?;
        }
        self.program_break
            .as_mut()
            .expect("validated program break state must remain installed")
            .current = new_break;
        Ok(new_break)
    }

    /// @description 为 RISC-V 共享地址空间中的新 Thread 分配独立 supervisor trap-context 页。
    ///
    /// @param tid 全局唯一且大于 init TID 的线程标识；只参与 RISC-V 临时 VA 投影。
    /// @return 成功返回该线程唯一 trap-context VA；冲突或溢出返回错误。
    pub(crate) fn allocate_thread_trap_context(
        &mut self,
        tid: usize,
    ) -> Result<usize, MemoryError> {
        let offset = tid
            .checked_mul(config::PAGE_SIZE)
            .ok_or(MemoryError::InvalidRange)?;
        let address = config::TRAP_CONTEXT
            .checked_sub(offset)
            .ok_or(MemoryError::InvalidRange)?;
        self.insert_framed_area(
            address.into(),
            (address + config::PAGE_SIZE).into(),
            MapPermission::R | MapPermission::W,
        )?;
        Ok(address)
    }

    /// @description 解除已退出 RISC-V Thread 的唯一 supervisor trap-context 页。
    ///
    /// @param address `allocate_thread_trap_context` 返回的页对齐地址。
    /// @return 无返回值；缺失映射表示退出清理重复并 fail-stop。
    pub(crate) fn remove_thread_trap_context(&mut self, address: usize) {
        let vpn = VirtualAddress::from(address).floor();
        self.remove_area_with_start_vpn(vpn);
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
