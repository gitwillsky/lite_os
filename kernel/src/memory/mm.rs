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
mod statistics;
mod user_access;
use super::config;
use super::permissions::MapPermission;
use super::retire::{reclaim_release_decision, revoke_and_synchronize};
use super::{address::VirtualPageNumber, page_table::PageTable};
use crate::fallible_tree::FallibleMap;
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
    pub(crate) fn new() -> Self {
        Self {
            page_table: PageTable::new(),
            areas: FallibleMap::new(),
            private_reclaim_cursor: VirtualPageNumber::from_vpn(0),
            code_range: 0..0,
            program_break: None,
            argument_range: 0..0,
        }
    }

    fn try_new() -> Result<Self, MemoryError> {
        Ok(Self {
            page_table: PageTable::try_new()?,
            areas: FallibleMap::new(),
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
        if self
            .areas
            .values()
            .any(|area| start < area.vpn_range.end && area.vpn_range.start < end)
            || self.areas.contains_key(&start)
        {
            return Err(MemoryError::AddressInUse);
        }
        // VMA node 必须先于 PTE publication 分配；缺少该 preflight 会在页表已经
        // 可见后因 ordered-index node OOM 无法记录其唯一 owner。
        let mut prepared = self
            .areas
            .try_prepare_vacant(start, map_area)
            .map_err(|_| MemoryError::OutOfMemory)?;
        if let Err(e) = prepared.value_mut().map(&mut self.page_table) {
            // VMA node 尚未发布，但 live shared mm 的 hardware walker 已可观察 partial PTE；
            // rollback 仍须保留 prepared area owner 直到所有 CPU 完成 fence。
            let prepared = revoke_and_synchronize(
                prepared,
                |prepared| {
                    prepared.value_mut().unmap(&mut self.page_table);
                },
                |_| Self::flush_tlb_all_cpus(),
            )
            .expect("platform TLB synchronization failed during VMA map rollback");
            drop(prepared);
            return Err(e);
        }
        if let Some(data) = data
            && let Err(error) = prepared.value_mut().copy_data(data)
        {
            let prepared = revoke_and_synchronize(
                prepared,
                |prepared| {
                    prepared.value_mut().unmap(&mut self.page_table);
                },
                |_| Self::flush_tlb_all_cpus(),
            )
            .expect("platform TLB synchronization failed during VMA data rollback");
            drop(prepared);
            return Err(error);
        }
        self.areas.commit_vacant(prepared);
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

    fn virtual_bytes(&self) -> u64 {
        self.areas
            .values()
            .filter(|area| area.map_permission.contains(MapPermission::U))
            .map(|area| {
                (area.vpn_range.end.as_usize() - area.vpn_range.start.as_usize()) as u64
                    * config::PAGE_SIZE as u64
            })
            .sum()
    }

    fn data_bytes(&self) -> u64 {
        self.areas
            .values()
            .filter(|area| {
                area.map_permission
                    .contains(MapPermission::U | MapPermission::W)
                    && area.shared_anonymous.is_none()
                    && area.shared_file.is_none()
                    && matches!(area.kind, VmaKind::Anonymous | VmaKind::Elf | VmaKind::File)
            })
            .map(|area| {
                (area.vpn_range.end.as_usize() - area.vpn_range.start.as_usize()) as u64
                    * config::PAGE_SIZE as u64
            })
            .sum()
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
        let strampoline_pa = PhysicalAddress::from(strampoline as *const () as usize);

        self.page_table.map(
            trampoline_va.into(),
            strampoline_pa.into(),
            // Trampoline 在所有地址空间通用，标记为 Global，避免跨进程切换时TLB混淆
            PagePermissions::READ | PagePermissions::EXECUTE | PagePermissions::GLOBAL,
        )?;
        self.page_table.map(
            VirtualAddress::from(config::SIGNAL_TRAMPOLINE).into(),
            strampoline_pa.into(),
            PagePermissions::READ | PagePermissions::EXECUTE | PagePermissions::USER,
        )?;
        Ok(())
    }

    pub(crate) fn active(&self) {
        crate::arch::mmu::activate(self.page_table.token());
    }

    /// @description 同步刷新所有 online CPU 的 userspace translation cache。
    ///
    /// @return 所有目标 CPU 完成 remote fence 后返回 `Ok(())`；platform 操作失败时返回错误。
    pub(crate) fn flush_tlb_all_cpus() -> Result<(), crate::platform::TlbShootdownError> {
        // 1. 当前 CPU 先完成 fence；当前页表写在后续 platform call 前保持程序顺序。
        crate::arch::mmu::flush_local();
        // 2. Acquire online set 只选择已发布可接收远端请求的 CPU。
        let mut targets = crate::cpu::online() & crate::cpu::possible();
        targets.remove(crate::cpu::current_id());
        if targets.is_empty() {
            return Ok(());
        }
        // 3. platform shootdown 是同步接口；返回即证明目标 CPU 已完成 fence。
        crate::platform::synchronize_tlb(targets, 0, 0)
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

    /// @description 为共享地址空间中的新 Thread 分配独立 supervisor trap-context 页。
    ///
    /// @param tid 全局唯一且大于 init TID 的线程标识。
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
        Self::flush_tlb_all_cpus()
            .expect("platform TLB synchronization failed after thread trap-context mapping");
        Ok(address)
    }

    /// @description 解除已退出 Thread 的唯一 supervisor trap-context 页。
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

#[derive(Debug, Clone)]
struct LoadedElf {
    entry: usize,
    phdr: Option<usize>,
    phent: usize,
    phnum: usize,
    max_end: usize,
    code_range: Range<usize>,
}
