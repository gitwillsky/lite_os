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
mod shared_area;
mod statistics;
mod user_access;
use super::config;
use super::{address::VirtualPageNumber, page_table::PageTable};
use crate::fallible_tree::FallibleMap;
use crate::memory::{
    ReclaimRequest, ReclaimResult, SharedFileError, SharedFileId, SharedFileMapping, SharedPage,
    address::{PhysicalAddress, PhysicalPageNumber, VirtualAddress},
    frame_allocator::{FrameTracker, alloc},
    page_table::{PTEFlags, PageTableEntry},
    strampoline,
};
use alloc::{sync::Arc, vec::Vec};
use bitflags::bitflags;
use core::{arch::asm, ops::Range};
use device_area::DeviceArea;
use error::try_memory_arc;
use fault_preflight::{
    FaultPermissions, FaultPreflight, FaultResidency, FileFaultState, preflight_fault,
};
use file_page_range::{FilePageRange, FilePageRangeError};
use private_area::{PrivateFaultPreparation, PrivateFileArea};
use resident::PrivateResident;
use riscv::register::satp::{self, Satp};
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
bitflags! {
    // PTE Flags 的子集
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct MapPermission: u8 {
        const R = 1 << 1; // 可读
        const W = 1 << 2; // 可写
        const X = 1 << 3; // 可执行
        const U = 1 << 4; // 用户态可访问 (默认仅 内核 态可访问)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum MapType {
    Identical, // PA <-> VA 恒等映射
    Framed,    // 映射到分配的物理页帧
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum VmaKind {
    System,
    Anonymous,
    Stack { top: usize },
    Elf,
    File,
    Device,
}

#[derive(Debug)]
pub(crate) struct MapArea {
    vpn_range: Range<VirtualPageNumber>,
    data_page_offset: usize,
    data_frames: FallibleMap<VirtualPageNumber, PrivateResident>,
    map_type: MapType,
    map_permission: MapPermission,
    /// 是否标记为全局页（G位）。仅用于内核空间映射。
    global: bool,
    kind: VmaKind,
    shared_anonymous: Option<SharedAnonymousArea>,
    shared_file: Option<SharedFileArea>,
    device: Option<DeviceArea>,
    private_file: Option<PrivateFileArea>,
    /// private VMA 只声明地址范围，首次访问才分配物理页。
    lazy_private: bool,
}

impl MapArea {
    fn has_leaf_permission(permission: MapPermission) -> bool {
        permission.intersects(MapPermission::R | MapPermission::W | MapPermission::X)
    }

    pub(crate) fn new(
        start_va: VirtualAddress,
        end_va: VirtualAddress,
        map_type: MapType,
        permissions: MapPermission,
    ) -> Self {
        let start_vpn = start_va.floor();
        let end_vpn = end_va.ceil();
        Self {
            vpn_range: Range {
                start: start_vpn,
                end: end_vpn,
            },
            data_page_offset: start_va.page_offset(),
            data_frames: FallibleMap::new(),
            map_permission: permissions,
            map_type,
            global: false,
            kind: VmaKind::System,
            shared_anonymous: None,
            shared_file: None,
            device: None,
            private_file: None,
            lazy_private: false,
        }
    }

    fn anonymous(
        start_va: VirtualAddress,
        end_va: VirtualAddress,
        permissions: MapPermission,
    ) -> Self {
        let mut area = Self::new(start_va, end_va, MapType::Framed, permissions);
        area.kind = VmaKind::Anonymous;
        area.lazy_private = true;
        area
    }

    fn elf(
        start_va: VirtualAddress,
        end_va: VirtualAddress,
        permissions: MapPermission,
        backing: PrivateFileArea,
    ) -> Self {
        let mut area = Self::new(start_va, end_va, MapType::Framed, permissions);
        area.kind = VmaKind::Elf;
        area.private_file = Some(backing);
        area.lazy_private = true;
        area
    }

    fn file(
        start_va: VirtualAddress,
        end_va: VirtualAddress,
        permissions: MapPermission,
        backing: PrivateFileArea,
    ) -> Self {
        let mut area = Self::new(start_va, end_va, MapType::Framed, permissions);
        area.kind = VmaKind::File;
        area.private_file = Some(backing);
        area.lazy_private = true;
        area
    }

    fn stack(top: usize) -> Self {
        let mut area = Self::new(
            (top - config::PAGE_SIZE).into(),
            top.into(),
            MapType::Framed,
            MapPermission::R | MapPermission::W | MapPermission::U,
        );
        area.kind = VmaKind::Stack { top };
        area.lazy_private = true;
        area
    }

    pub(crate) fn set_global(mut self, global: bool) -> Self {
        self.global = global;
        self
    }

    fn copy_data(&mut self, data: &[u8]) -> Result<(), MemoryError> {
        if self.map_type != MapType::Framed {
            return Err(MemoryError::InvalidRange);
        }
        let capacity = self
            .data_frames
            .len()
            .checked_mul(config::PAGE_SIZE)
            .and_then(|bytes| bytes.checked_sub(self.data_page_offset))
            .ok_or(MemoryError::InvalidRange)?;
        if data.len() > capacity {
            return Err(MemoryError::InvalidRange);
        }

        let mut copied = 0usize;
        let mut index = 0usize;
        self.data_frames.for_each_mut(|_, frame| {
            if copied == data.len() {
                return;
            }
            let page_offset = if index == 0 { self.data_page_offset } else { 0 };
            let count = (config::PAGE_SIZE - page_offset).min(data.len() - copied);
            Arc::get_mut(frame)
                .expect("new mapping frame must be uniquely owned")
                .bytes_mut()[page_offset..page_offset + count]
                .copy_from_slice(&data[copied..copied + count]);
            copied += count;
            index += 1;
        });
        (copied == data.len())
            .then_some(())
            .ok_or(MemoryError::InvalidRange)
    }

    pub(crate) fn map(&mut self, page_table: &mut PageTable) -> Result<(), MemoryError> {
        if self.device.is_some() {
            return self.map_device_area(page_table);
        }
        if self.shared_file.is_some() {
            let _ = page_table;
            return Ok(());
        }
        if self.map_shared_anonymous(page_table)? {
            return Ok(());
        }
        if self.lazy_private {
            return Ok(());
        }
        for vpn in self.vpn_range.start.as_usize()..self.vpn_range.end.as_usize() {
            self.map_one(page_table, VirtualPageNumber::from_vpn(vpn))?;
        }
        Ok(())
    }

    pub(crate) fn unmap(&mut self, page_table: &mut PageTable) {
        if self.device.is_some() {
            self.unmap_device_area(page_table);
            return;
        }
        if self.lazy_private || self.shared_anonymous.is_some() || self.shared_file.is_some() {
            for (&vpn, _) in &self.data_frames {
                let _ = page_table.unmap(vpn);
            }
            if let Some(shared) = &self.shared_file {
                for (&vpn, _) in &shared.resident {
                    let _ = page_table.unmap(vpn);
                }
            }
        } else {
            for vpn in self.vpn_range.start.as_usize()..self.vpn_range.end.as_usize() {
                let _ = page_table.unmap(VirtualPageNumber::from_vpn(vpn));
            }
        }
        self.data_frames.clear();
        if let Some(shared) = &mut self.shared_file {
            shared.resident.clear();
        }
    }

    fn map_one(
        &mut self,
        page_table: &mut PageTable,
        vpn: VirtualPageNumber,
    ) -> Result<(), MemoryError> {
        let (ppn, frame) = match self.map_type {
            MapType::Framed => {
                let frame = try_memory_arc(alloc().ok_or(MemoryError::OutOfMemory)?)?;
                (frame.ppn, Some(frame))
            }
            MapType::Identical => (vpn.as_usize().into(), None),
        };

        let resident = frame
            .map(|frame| {
                self.data_frames
                    .try_prepare_vacant(vpn, PrivateResident::new(frame))
            })
            .transpose()
            .map_err(|_| MemoryError::OutOfMemory)?;

        if Self::has_leaf_permission(self.map_permission) {
            let mut pte_flags = PTEFlags::from_bits(self.map_permission.bits()).unwrap();
            if self.global {
                pte_flags |= PTEFlags::G;
            }
            page_table.map(vpn, ppn, pte_flags)?;
        } else if self.map_type == MapType::Framed {
            // PROT_NONE VMA 仍由 data_frames 唯一持有物理页，但 leaf slot 必须保持 invalid。
            // 若写入 V|U 且 R/W/X 全零，RISC-V walker 会把数据页误当成下一级页表。
            page_table.reserve(vpn)?;
        } else {
            return Err(MemoryError::InvalidRange);
        }
        if let Some(resident) = resident {
            self.data_frames.commit_vacant(resident);
        }
        Ok(())
    }

    fn partition_protectable(
        mut self,
        start: VirtualPageNumber,
        end: VirtualPageNumber,
    ) -> (Option<Self>, Self, Option<Self>) {
        debug_assert!(matches!(
            self.kind,
            VmaKind::Anonymous | VmaKind::Elf | VmaKind::File | VmaKind::Device
        ));
        debug_assert!(self.vpn_range.start <= start && end <= self.vpn_range.end);
        let original_start = self.vpn_range.start;
        let original_end = self.vpn_range.end;
        let right_frames = self.data_frames.split_off(&end);
        let middle_frames = self.data_frames.split_off(&start);
        let (left_shared, middle_shared, right_shared) =
            SharedFileArea::partition(self.shared_file, original_start..original_end, start..end);
        let (left_anonymous, middle_anonymous, right_anonymous) =
            SharedAnonymousArea::partition(self.shared_anonymous, original_start, start, end);
        let (left_device, middle_device, right_device) =
            DeviceArea::partition(self.device, original_start, start, end);
        let kind = self.kind;
        let build = |range: Range<VirtualPageNumber>,
                     data_frames,
                     shared_anonymous,
                     shared_file,
                     device| Self {
            vpn_range: range,
            data_page_offset: 0,
            data_frames,
            map_type: MapType::Framed,
            map_permission: self.map_permission,
            global: false,
            kind,
            shared_anonymous,
            shared_file,
            device,
            private_file: self.private_file.clone(),
            lazy_private: self.lazy_private,
        };
        let left = (original_start < start).then(|| {
            build(
                original_start..start,
                self.data_frames,
                left_anonymous,
                left_shared,
                left_device,
            )
        });
        let middle = build(
            start..end,
            middle_frames,
            middle_anonymous,
            middle_shared,
            middle_device,
        );
        let right = (end < original_end).then(|| {
            build(
                end..original_end,
                right_frames,
                right_anonymous,
                right_shared,
                right_device,
            )
        });
        (left, middle, right)
    }

    fn merge_anonymous(&mut self, mut right: Self) {
        debug_assert_eq!(self.kind, VmaKind::Anonymous);
        debug_assert_eq!(right.kind, VmaKind::Anonymous);
        debug_assert_eq!(self.vpn_range.end, right.vpn_range.start);
        debug_assert_eq!(self.map_permission, right.map_permission);
        self.vpn_range.end = right.vpn_range.end;
        self.data_frames.append(&mut right.data_frames);
    }
}

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
            // 回滚：解除已经映射的页面
            prepared.value_mut().unmap(&mut self.page_table);
            return Err(e);
        }
        if let Some(data) = data
            && let Err(error) = prepared.value_mut().copy_data(data)
        {
            prepared.value_mut().unmap(&mut self.page_table);
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

    pub(crate) fn token(&self) -> usize {
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
            PTEFlags::R | PTEFlags::X | PTEFlags::G,
        )?;
        self.page_table.map(
            VirtualAddress::from(config::SIGNAL_TRAMPOLINE).into(),
            strampoline_pa.into(),
            PTEFlags::R | PTEFlags::X | PTEFlags::U,
        )?;
        Ok(())
    }

    pub(crate) fn active(&self) {
        let satp = self.page_table.token();
        // SAFETY: token encodes this live Sv39 root table; activation runs in S-mode and the
        // following local fence invalidates translations derived from the previous root.
        unsafe {
            satp::write(Satp::from_bits(satp));
            asm!("sfence.vma")
        }
    }

    /// @description 同步刷新所有 online hart 的 S-stage TLB。
    ///
    /// @return 所有目标 hart 完成 `SFENCE.VMA` 后返回 `Ok(())`；SBI RFENCE 失败时返回错误码。
    pub(crate) fn flush_tlb_all_cpus() -> Result<(), isize> {
        // 1. 本 hart 先完成 fence；当前页表写在后续 SBI ecall 之前保持程序顺序。
        // SAFETY: `sfence.vma` is executed in S-mode and affects only architectural TLB state.
        unsafe { asm!("sfence.vma") }
        // 2. Acquire online mask 只选择已发布可接收远端请求的 hart。
        let current = crate::arch::hart::hart_id();
        let targets = crate::arch::hart::online_hart_mask()
            & crate::arch::hart::possible_hart_mask()
            & !(1usize << current);
        if targets == 0 {
            return Ok(());
        }
        // 3. SBI RFENCE 是同步接口；返回即证明目标 hart 已完成 fence。
        crate::arch::sbi::remote_sfence_vma(targets, 0, 0, 0)
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

    pub(crate) fn remove_area_with_start_vpn(&mut self, start_vpn: VirtualPageNumber) {
        if let Some(mut area) = self.areas.remove(&start_vpn) {
            // 将目标区域移出容器后再执行 unmap，规避潜在别名问题
            area.unmap(&mut self.page_table);
        }
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
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after thread trap-context mapping");
        Ok(address)
    }

    /// @description 解除已退出 Thread 的唯一 supervisor trap-context 页。
    ///
    /// @param address `allocate_thread_trap_context` 返回的页对齐地址。
    /// @return 无返回值；缺失映射表示退出清理重复并 fail-stop。
    pub(crate) fn remove_thread_trap_context(&mut self, address: usize) {
        let vpn = VirtualAddress::from(address).floor();
        assert!(self.areas.contains_key(&vpn));
        self.remove_area_with_start_vpn(vpn);
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after thread trap-context unmapping");
    }

    /// 获取给定TrapContext虚拟地址的物理页号
    pub(crate) fn trap_context_ppn(&self, trap_va: usize) -> PhysicalPageNumber {
        self.page_table
            .translate(VirtualAddress::from(trap_va).into())
            .expect("TrapContext VA should be mapped")
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
