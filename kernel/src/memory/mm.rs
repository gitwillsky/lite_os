use core::sync::atomic::{AtomicU32, Ordering};
use core::{arch::asm, error::Error, ops::Range};

use alloc::{collections::BTreeMap, vec::Vec};
use bitflags::bitflags;
use riscv::register::satp::{self, Satp};

use crate::memory::{
    address::{PhysicalAddress, PhysicalPageNumber, VirtualAddress},
    frame_allocator::{FrameTracker, alloc},
    page_table::{PTEFlags, PageTableEntry, PageTableError},
    strampoline,
};

use super::config;
use super::{address::VirtualPageNumber, page_table::PageTable};

#[derive(Debug, Clone, Copy)]
pub(crate) enum MemoryError {
    OutOfMemory,
    PageTableError(PageTableError),
    InvalidRange,
    AddressInUse,
    PermissionDenied,
}

/// @description 用户地址复制失败原因；所有成员都表示不能完成完整 copyin/copyout。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UserAccessError {
    /// 地址为空、非用户 canonical 地址、未映射或权限不匹配。
    Fault,
    /// 地址加长度发生整数溢出。
    Overflow,
    /// 在调用方指定上限内没有找到 NUL。
    Unterminated,
    /// 无法为 kernel-owned copy 缓冲区分配内存。
    OutOfMemory,
}

impl core::fmt::Display for UserAccessError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Fault => write!(f, "invalid user address or permission"),
            Self::Overflow => write!(f, "user address range overflow"),
            Self::Unterminated => write!(f, "unterminated user string"),
            Self::OutOfMemory => write!(f, "out of memory while copying user string"),
        }
    }
}

impl Error for UserAccessError {}

impl From<PageTableError> for MemoryError {
    fn from(err: PageTableError) -> Self {
        MemoryError::PageTableError(err)
    }
}

impl core::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            MemoryError::OutOfMemory => write!(f, "Out of memory"),
            MemoryError::PageTableError(err) => write!(f, "Page table error: {err}"),
            MemoryError::InvalidRange => write!(f, "Invalid virtual memory range"),
            MemoryError::AddressInUse => write!(f, "Virtual memory range is already mapped"),
            MemoryError::PermissionDenied => write!(f, "Virtual memory operation is not allowed"),
        }
    }
}

impl Error for MemoryError {}

impl MemoryError {
    /// @description 判断失败是否来自物理页或页表页资源耗尽，不向上层泄漏页表错误类型。
    ///
    /// @return 资源耗尽返回 true，其他地址或权限错误返回 false。
    pub(crate) fn is_out_of_memory(self) -> bool {
        matches!(
            self,
            Self::OutOfMemory | Self::PageTableError(PageTableError::OutOfMemory)
        )
    }
}

/// @description 构造新用户映像时需要暴露给 `execve` 的失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElfLoadError {
    /// 物理页或页表页分配失败。
    OutOfMemory,
    /// ELF header、segment、地址、权限、解释器或初始栈不满足 RV64 契约。
    InvalidElf,
}

/// @description exec transaction 的主 ELF 与可选 PT_INTERP file image。
pub(crate) struct ExecutableImage {
    pub(crate) main: Vec<u8>,
    pub(crate) interpreter: Option<Vec<u8>>,
}

impl From<MemoryError> for ElfLoadError {
    fn from(error: MemoryError) -> Self {
        match error {
            MemoryError::OutOfMemory | MemoryError::PageTableError(PageTableError::OutOfMemory) => {
                Self::OutOfMemory
            }
            MemoryError::PageTableError(_)
            | MemoryError::InvalidRange
            | MemoryError::AddressInUse
            | MemoryError::PermissionDenied => Self::InvalidElf,
        }
    }
}

impl core::fmt::Display for ElfLoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutOfMemory => write!(f, "out of memory while loading ELF"),
            Self::InvalidElf => write!(f, "invalid or unsupported RV64 ELF image"),
        }
    }
}

impl Error for ElfLoadError {}

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
    Heap {
        base: usize,
        program_break: usize,
        limit: usize,
    },
    Anonymous,
    Elf,
    File,
}

#[derive(Debug)]
pub(crate) struct MapArea {
    vpn_range: Range<VirtualPageNumber>,
    data_page_offset: usize,
    data_frames: BTreeMap<VirtualPageNumber, FrameTracker>,
    map_type: MapType,
    map_permission: MapPermission,
    /// 是否标记为全局页（G位）。仅用于内核空间映射。
    global: bool,
    kind: VmaKind,
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
            data_frames: BTreeMap::new(),
            map_permission: permissions,
            map_type,
            global: false,
            kind: VmaKind::System,
        }
    }

    fn anonymous(
        start_va: VirtualAddress,
        end_va: VirtualAddress,
        permissions: MapPermission,
    ) -> Self {
        let mut area = Self::new(start_va, end_va, MapType::Framed, permissions);
        area.kind = VmaKind::Anonymous;
        area
    }

    fn elf(start_va: VirtualAddress, end_va: VirtualAddress, permissions: MapPermission) -> Self {
        let mut area = Self::new(start_va, end_va, MapType::Framed, permissions);
        area.kind = VmaKind::Elf;
        area
    }

    fn file(start_va: VirtualAddress, end_va: VirtualAddress, permissions: MapPermission) -> Self {
        let mut area = Self::new(start_va, end_va, MapType::Framed, permissions);
        area.kind = VmaKind::File;
        area
    }

    fn heap(base: usize, limit: usize) -> Self {
        let mut area = Self::new(
            base.into(),
            base.into(),
            MapType::Framed,
            MapPermission::R | MapPermission::W | MapPermission::U,
        );
        area.kind = VmaKind::Heap {
            base,
            program_break: base,
            limit,
        };
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
        for (index, frame) in self.data_frames.values_mut().enumerate() {
            let page_offset = if index == 0 { self.data_page_offset } else { 0 };
            let count = (config::PAGE_SIZE - page_offset).min(data.len() - copied);
            frame.bytes_mut()[page_offset..page_offset + count]
                .copy_from_slice(&data[copied..copied + count]);
            copied += count;
            if copied == data.len() {
                return Ok(());
            }
        }
        (copied == data.len())
            .then_some(())
            .ok_or(MemoryError::InvalidRange)
    }

    pub(crate) fn map(&mut self, page_table: &mut PageTable) -> Result<(), MemoryError> {
        for vpn in self.vpn_range.start.as_usize()..self.vpn_range.end.as_usize() {
            self.map_one(page_table, VirtualPageNumber::from_vpn(vpn))?;
        }
        Ok(())
    }

    pub(crate) fn unmap(&mut self, page_table: &mut PageTable) {
        let start = self.vpn_range.start.as_usize();
        let end = self.vpn_range.end.as_usize();

        for vpn_usize in start..end {
            let vpn = VirtualPageNumber::from_vpn(vpn_usize);
            let _ = page_table.unmap(vpn);
        }
        self.data_frames.clear();
    }

    fn map_one(
        &mut self,
        page_table: &mut PageTable,
        vpn: VirtualPageNumber,
    ) -> Result<(), MemoryError> {
        let (ppn, frame) = match self.map_type {
            MapType::Framed => {
                let frame = alloc().ok_or(MemoryError::OutOfMemory)?;
                (frame.ppn, Some(frame))
            }
            MapType::Identical => (vpn.as_usize().into(), None),
        };

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
        if let Some(frame) = frame {
            let replaced = self.data_frames.insert(vpn, frame);
            debug_assert!(replaced.is_none());
        }
        Ok(())
    }

    fn unmap_one(&mut self, page_table: &mut PageTable, vpn: VirtualPageNumber) {
        let _ = page_table.unmap(vpn);
        self.data_frames.remove(&vpn);
    }

    pub(crate) fn shrink_to(
        &mut self,
        page_table: &mut PageTable,
        new_end: VirtualPageNumber,
    ) -> Result<(), MemoryError> {
        if new_end < self.vpn_range.start || new_end > self.vpn_range.end {
            return Err(MemoryError::InvalidRange);
        }
        for vpn in new_end.as_usize()..self.vpn_range.end.as_usize() {
            self.unmap_one(page_table, VirtualPageNumber::from_vpn(vpn));
        }
        self.vpn_range = Range {
            start: self.vpn_range.start,
            end: new_end,
        };
        Ok(())
    }

    pub(crate) fn append_to(
        &mut self,
        page_table: &mut PageTable,
        new_end: VirtualPageNumber,
    ) -> Result<(), MemoryError> {
        if new_end < self.vpn_range.end {
            return Err(MemoryError::InvalidRange);
        }
        let old_end = self.vpn_range.end;
        for vpn in old_end.as_usize()..new_end.as_usize() {
            let vpn = VirtualPageNumber::from_vpn(vpn);
            if let Err(error) = self.map_one(page_table, vpn) {
                for rollback in old_end.as_usize()..vpn.as_usize() {
                    self.unmap_one(page_table, VirtualPageNumber::from_vpn(rollback));
                }
                return Err(error);
            }
        }
        self.vpn_range = Range {
            start: self.vpn_range.start,
            end: new_end,
        };
        Ok(())
    }

    fn partition_protectable(
        mut self,
        start: VirtualPageNumber,
        end: VirtualPageNumber,
    ) -> (Option<Self>, Self, Option<Self>) {
        debug_assert!(matches!(
            self.kind,
            VmaKind::Anonymous | VmaKind::Elf | VmaKind::File
        ));
        debug_assert!(self.vpn_range.start <= start && end <= self.vpn_range.end);
        let original_start = self.vpn_range.start;
        let original_end = self.vpn_range.end;
        let right_frames = self.data_frames.split_off(&end);
        let middle_frames = self.data_frames.split_off(&start);
        let kind = self.kind;
        let build = |range: Range<VirtualPageNumber>, data_frames| Self {
            vpn_range: range,
            data_page_offset: 0,
            data_frames,
            map_type: MapType::Framed,
            map_permission: self.map_permission,
            global: false,
            kind,
        };
        let left = (original_start < start).then(|| build(original_start..start, self.data_frames));
        let middle = build(start..end, middle_frames);
        let right = (end < original_end).then(|| build(end..original_end, right_frames));
        (left, middle, right)
    }

    fn merge_anonymous(mut self, mut right: Self) -> Self {
        debug_assert_eq!(self.kind, VmaKind::Anonymous);
        debug_assert_eq!(right.kind, VmaKind::Anonymous);
        debug_assert_eq!(self.vpn_range.end, right.vpn_range.start);
        debug_assert_eq!(self.map_permission, right.map_permission);
        self.vpn_range.end = right.vpn_range.end;
        self.data_frames.append(&mut right.data_frames);
        self
    }

    fn try_clone_into(&self, page_table: &mut PageTable) -> Result<Self, MemoryError> {
        let mut cloned = Self {
            vpn_range: self.vpn_range.clone(),
            data_page_offset: self.data_page_offset,
            data_frames: BTreeMap::new(),
            map_type: self.map_type,
            map_permission: self.map_permission,
            global: self.global,
            kind: self.kind,
        };
        if let Err(error) = cloned.map(page_table) {
            cloned.unmap(page_table);
            return Err(error);
        }
        for (vpn, source) in &self.data_frames {
            cloned
                .data_frames
                .get_mut(vpn)
                .expect("cloned framed VMA must own every source VPN")
                .bytes_mut()
                .copy_from_slice(source.bytes());
        }
        Ok(cloned)
    }
}

#[derive(Debug)]
pub(crate) struct MemorySet {
    page_table: PageTable,
    areas: BTreeMap<VirtualPageNumber, MapArea>,
}

impl MemorySet {
    const MMAP_BASE: usize = 0x4000_0000;

    pub(crate) fn new() -> Self {
        Self {
            page_table: PageTable::new(),
            areas: BTreeMap::new(),
        }
    }

    fn try_new() -> Result<Self, MemoryError> {
        Ok(Self {
            page_table: PageTable::try_new()?,
            areas: BTreeMap::new(),
        })
    }

    pub(crate) fn push(
        &mut self,
        mut map_area: MapArea,
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
        // 先尝试映射；若中途失败，需要回滚已映射页面，保持 VMA 表与页表同时不变。
        if let Err(e) = map_area.map(&mut self.page_table) {
            // 回滚：解除已经映射的页面
            map_area.unmap(&mut self.page_table);
            return Err(e);
        }
        if let Some(data) = data
            && let Err(error) = map_area.copy_data(data)
        {
            map_area.unmap(&mut self.page_table);
            return Err(error);
        }
        self.areas.insert(start, map_area);
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

    /// @description 统计当前用户 VMA 的虚拟页数与已驻留物理页数。
    ///
    /// @return `(virtual_pages, resident_pages)`；不计 kernel-only trap context。
    pub(crate) fn user_page_statistics(&self) -> (usize, usize) {
        self.areas
            .values()
            .filter(|area| area.map_permission.contains(MapPermission::U))
            .fold((0, 0), |(virtual_pages, resident_pages), area| {
                (
                    virtual_pages + area.vpn_range.end.as_usize() - area.vpn_range.start.as_usize(),
                    resident_pages + area.data_frames.len(),
                )
            })
    }

    /// @description 为 fork eager 深拷贝完整用户地址空间，保留 VMA 元数据但不共享物理页。
    ///
    /// @return 成功返回独立页表与 frame owner；OOM 时释放全部已复制 VMA。
    pub(crate) fn try_clone_for_fork(&self) -> Result<Self, MemoryError> {
        let mut cloned = Self::try_new()?;
        cloned.map_trampoline()?;
        for (key, area) in &self.areas {
            let cloned_area = area.try_clone_into(&mut cloned.page_table)?;
            assert!(cloned.areas.insert(*key, cloned_area).is_none());
        }
        Ok(cloned)
    }

    pub(crate) fn map_trampoline(&mut self) -> Result<(), MemoryError> {
        let trampoline_va = VirtualAddress::from(config::TRAMPOLINE);
        let strampoline_pa = PhysicalAddress::from(strampoline as usize);

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

    fn checked_user_end(start: usize, len: usize) -> Result<usize, UserAccessError> {
        if len == 0 {
            return Ok(start);
        }
        let end = start.checked_add(len).ok_or(UserAccessError::Overflow)?;
        let user_end = 1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1);
        if start == 0 || start >= user_end || end > user_end {
            return Err(UserAccessError::Fault);
        }
        Ok(end)
    }

    fn user_page(
        &self,
        address: usize,
        required: PTEFlags,
    ) -> Result<(PhysicalPageNumber, usize), UserAccessError> {
        let va = VirtualAddress::from(address);
        let pte = self
            .page_table
            .translate(va.floor())
            .ok_or(UserAccessError::Fault)?;
        let flags = pte.flags();
        if !flags.contains(PTEFlags::U | required) {
            return Err(UserAccessError::Fault);
        }
        Ok((pte.ppn(), va.page_offset()))
    }

    fn validate_user_range(
        &self,
        start: usize,
        len: usize,
        required: PTEFlags,
    ) -> Result<usize, UserAccessError> {
        let end = Self::checked_user_end(start, len)?;
        let mut current = start;
        while current < end {
            let (_, page_offset) = self.user_page(current, required)?;
            current += (config::PAGE_SIZE - page_offset).min(end - current);
        }
        Ok(end)
    }

    /// @description 从当前地址空间复制用户字节到 kernel-owned 缓冲区。
    ///
    /// @param user_address 用户缓冲区首地址。
    /// @param destination kernel 目标缓冲区。
    /// @return 完整复制成功返回 `Ok(())`；地址溢出、缺页或非 `U|R` leaf 返回错误，且不返回用户引用。
    pub(crate) fn copy_from_user(
        &self,
        user_address: usize,
        destination: &mut [u8],
    ) -> Result<(), UserAccessError> {
        // 1. 先验证完整范围，避免尾页 fault 时 destination 只得到前缀数据。
        let end = self.validate_user_range(user_address, destination.len(), PTEFlags::R)?;
        // 2. 验证成功后逐页复制；本阶段没有 lazy fault，映射在 &self 生命周期内稳定。
        let mut current = user_address;
        let mut copied = 0usize;
        while current < end {
            let (ppn, page_offset) = self.user_page(current, PTEFlags::R)?;
            let count = (config::PAGE_SIZE - page_offset).min(end - current);
            // SAFETY: user_page 证明源页在本 MemorySet 中以 U|R leaf 映射；&self 保证
            // 映射和 FrameTracker 在调用期间不被修改/回收。destination 是有效独占切片。
            unsafe {
                core::ptr::copy(
                    ppn.as_page_ptr().add(page_offset),
                    destination.as_mut_ptr().add(copied),
                    count,
                );
            }
            current += count;
            copied += count;
        }
        Ok(())
    }

    /// @description 将 kernel-owned 字节复制到当前地址空间的用户缓冲区。
    ///
    /// @param user_address 用户缓冲区首地址。
    /// @param source kernel 源缓冲区。
    /// @return 完整复制成功返回 `Ok(())`；地址溢出、缺页或非 `U|W` leaf 返回错误，且不返回用户引用。
    pub(crate) fn copy_to_user(
        &mut self,
        user_address: usize,
        source: &[u8],
    ) -> Result<(), UserAccessError> {
        // 1. 先验证完整范围，避免尾页 fault 时用户缓冲区被部分修改。
        let end = self.validate_user_range(user_address, source.len(), PTEFlags::W)?;
        // 2. 验证成功后逐页复制；&mut self 阻止并发 unmap/permission change。
        let mut current = user_address;
        let mut copied = 0usize;
        while current < end {
            let (ppn, page_offset) = self.user_page(current, PTEFlags::W)?;
            let count = (config::PAGE_SIZE - page_offset).min(end - current);
            // SAFETY: user_page 证明目标页在本 MemorySet 中以 U|W leaf 映射；&mut self
            // 保证软件写访问独占且映射存活。source 是有效只读切片。
            unsafe {
                core::ptr::copy(
                    source.as_ptr().add(copied),
                    ppn.as_page_mut_ptr().add(page_offset),
                    count,
                );
            }
            current += count;
            copied += count;
        }
        Ok(())
    }

    /// @description 验证完整用户范围当前可写，不修改用户数据。
    ///
    /// @param address 用户范围首地址。
    /// @param length 字节长度。
    /// @return 全部页面具有 U|W 返回成功，否则返回 fault/overflow。
    pub(crate) fn validate_user_write(
        &self,
        address: usize,
        length: usize,
    ) -> Result<(), UserAccessError> {
        self.validate_user_range(address, length, PTEFlags::W)
            .map(|_| ())
    }

    pub(crate) fn compare_exchange_user_u32(
        &mut self,
        address: usize,
        current: u32,
        new: u32,
    ) -> Result<Result<u32, u32>, UserAccessError> {
        if address & 3 != 0 {
            return Err(UserAccessError::Fault);
        }
        Self::checked_user_end(address, 4)?;
        let (ppn, offset) = self.user_page(address, PTEFlags::R | PTEFlags::W)?;
        if offset + 4 > config::PAGE_SIZE {
            return Err(UserAccessError::Fault);
        }
        // SAFETY: user_page validates a live U|R|W page, alignment is checked, and the
        // AddressSpace lock keeps the mapping/frame alive for the atomic operation.
        let atomic = unsafe { &*ppn.as_page_ptr().add(offset).cast::<AtomicU32>() };
        Ok(atomic.compare_exchange(current, new, Ordering::AcqRel, Ordering::Acquire))
    }

    /// @description 从用户空间复制有长度上限的 NUL 结尾字节串。
    ///
    /// @param user_address 字符串首地址。
    /// @param max_len 包含终止 NUL 的最大总字节数。
    /// @return 成功返回不含 NUL 的 owned bytes；fault、未终止或内存不足返回明确错误。
    pub(crate) fn copy_user_c_string(
        &self,
        user_address: usize,
        max_len: usize,
    ) -> Result<Vec<u8>, UserAccessError> {
        if user_address == 0 {
            return Err(UserAccessError::Fault);
        }
        let mut bytes = Vec::new();
        let mut current = user_address;
        while bytes.len() < max_len {
            Self::checked_user_end(current, 1)?;
            let (ppn, page_offset) = self.user_page(current, PTEFlags::R)?;
            let count = (config::PAGE_SIZE - page_offset).min(max_len - bytes.len());
            // SAFETY: user_page 证明本页在 MemorySet 存活期间可读；切片只在本次循环
            // 内使用且不会逃逸，长度限制在当前 4 KiB 页内。
            let page =
                unsafe { core::slice::from_raw_parts(ppn.as_page_ptr().add(page_offset), count) };
            if let Some(nul) = page.iter().position(|byte| *byte == 0) {
                bytes
                    .try_reserve_exact(nul)
                    .map_err(|_| UserAccessError::OutOfMemory)?;
                bytes.extend_from_slice(&page[..nul]);
                return Ok(bytes);
            }
            bytes
                .try_reserve_exact(count)
                .map_err(|_| UserAccessError::OutOfMemory)?;
            bytes.extend_from_slice(page);
            current = current
                .checked_add(count)
                .ok_or(UserAccessError::Overflow)?;
        }
        Err(UserAccessError::Unterminated)
    }

    fn initialize_user_heap(&mut self, base: usize, limit: usize) -> Result<(), MemoryError> {
        if base >= limit || !VirtualAddress::from(base).is_aligned() {
            return Err(MemoryError::InvalidRange);
        }
        self.push(MapArea::heap(base, limit), None)
    }

    /// @description 查询或原子提交当前用户地址空间的 program break。
    ///
    /// @param new_break 新 break；零表示只查询。
    /// @return 成功返回提交后的 break；越界、映射冲突或 OOM 时返回错误且保持旧 break。
    pub(crate) fn set_program_break(&mut self, new_break: usize) -> Result<usize, MemoryError> {
        let heap_key = self
            .areas
            .iter()
            .find_map(|(key, area)| matches!(area.kind, VmaKind::Heap { .. }).then_some(*key))
            .ok_or(MemoryError::InvalidRange)?;
        let (base, old_break, limit, old_page_end) = match self.areas[&heap_key].kind {
            VmaKind::Heap {
                base,
                program_break,
                limit,
            } => (
                base,
                program_break,
                limit,
                self.areas[&heap_key].vpn_range.end,
            ),
            _ => return Err(MemoryError::InvalidRange),
        };
        if new_break == 0 {
            return Ok(old_break);
        }
        if new_break < base || new_break > limit {
            return Err(MemoryError::InvalidRange);
        }
        let new_page_end = VirtualAddress::from(new_break).ceil();
        if new_page_end > old_page_end
            && self
                .areas
                .range((
                    core::ops::Bound::Excluded(heap_key),
                    core::ops::Bound::Unbounded,
                ))
                .next()
                .is_some_and(|(next, _)| new_page_end > *next)
        {
            return Err(MemoryError::AddressInUse);
        }
        let area = self
            .areas
            .get_mut(&heap_key)
            .ok_or(MemoryError::InvalidRange)?;
        if new_page_end > old_page_end {
            area.append_to(&mut self.page_table, new_page_end)?;
        } else if new_page_end < old_page_end {
            area.shrink_to(&mut self.page_table, new_page_end)?;
        }
        if let VmaKind::Heap { program_break, .. } = &mut area.kind {
            *program_break = new_break;
        }

        if new_page_end != old_page_end {
            Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after brk page-table update");
        }
        Ok(new_break)
    }

    fn range_is_free(&self, start: VirtualPageNumber, end: VirtualPageNumber) -> bool {
        start < end
            && !self
                .areas
                .values()
                .any(|area| start < area.vpn_range.end && area.vpn_range.start < end)
    }

    fn find_free_user_range(
        &self,
        first: VirtualPageNumber,
        page_count: usize,
    ) -> Option<Range<VirtualPageNumber>> {
        let user_end = (1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1)) / config::PAGE_SIZE;
        let mut start = first.as_usize().max(1);
        for area in self.areas.values() {
            let area_start = area.vpn_range.start.as_usize();
            let area_end = area.vpn_range.end.as_usize();
            if area_end <= start {
                continue;
            }
            if let Some(end) = start.checked_add(page_count)
                && end <= area_start.min(user_end)
            {
                return Some(start.into()..end.into());
            }
            start = start.max(area_end);
            if start >= user_end {
                return None;
            }
        }
        let end = start.checked_add(page_count)?;
        (end <= user_end).then(|| start.into()..end.into())
    }

    /// @description 建立 eager anonymous private 用户映射，VMA 表是区间与页帧的唯一 owner。
    ///
    /// @param address 零表示由内核选址；非零是 page-aligned hint 或 fixed-noreplace 地址。
    /// @param length 非零字节长度，向上取整到整页。
    /// @param permission 用户页权限；必须含 U，允许 PROT_NONE，禁止 W+X。
    /// @param fixed_noreplace 为真时地址冲突返回 `AddressInUse`，不替换既有 VMA。
    /// @return 成功返回 page-aligned 起始地址；任何失败都不改变页表或 VMA 表。
    pub(crate) fn map_anonymous(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
    ) -> Result<usize, MemoryError> {
        if length == 0
            || !permission.contains(MapPermission::U)
            || permission.contains(MapPermission::W | MapPermission::X)
            || (fixed_noreplace && (address == 0 || !VirtualAddress::from(address).is_aligned()))
        {
            return Err(MemoryError::InvalidRange);
        }
        let page_count = length
            .checked_add(config::PAGE_SIZE - 1)
            .ok_or(MemoryError::InvalidRange)?
            / config::PAGE_SIZE;
        let hinted_start = VirtualAddress::from(address).floor();
        let hinted_end = hinted_start
            .as_usize()
            .checked_add(page_count)
            .map(VirtualPageNumber::from_vpn)
            .ok_or(MemoryError::InvalidRange)?;
        let user_end_vpn = (1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1)) / config::PAGE_SIZE;
        let hint_is_valid = address != 0
            && hinted_start.as_usize() < user_end_vpn
            && hinted_end.as_usize() <= user_end_vpn;
        let range = if hint_is_valid && self.range_is_free(hinted_start, hinted_end) {
            hinted_start..hinted_end
        } else if fixed_noreplace {
            return Err(if hint_is_valid {
                MemoryError::AddressInUse
            } else {
                MemoryError::InvalidRange
            });
        } else {
            self.find_free_user_range(VirtualAddress::from(Self::MMAP_BASE).floor(), page_count)
                .ok_or(MemoryError::OutOfMemory)?
        };
        let start_address = usize::from(VirtualAddress::from(range.start));
        let end_address = usize::from(VirtualAddress::from(range.end));
        self.push(
            MapArea::anonymous(start_address.into(), end_address.into(), permission),
            None,
        )?;
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after mmap page-table update");
        Ok(start_address)
    }

    /// @description 建立 eager file-backed private 映射；VMA 独占映射后的私有页帧。
    pub(crate) fn map_private_file(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        data: &[u8],
    ) -> Result<usize, MemoryError> {
        if length == 0
            || data.len() > length
            || !permission.contains(MapPermission::U)
            || permission.contains(MapPermission::W | MapPermission::X)
            || (fixed_noreplace && (address == 0 || !VirtualAddress::from(address).is_aligned()))
        {
            return Err(MemoryError::InvalidRange);
        }
        let page_count = length
            .checked_add(config::PAGE_SIZE - 1)
            .ok_or(MemoryError::InvalidRange)?
            / config::PAGE_SIZE;
        let hinted_start = VirtualAddress::from(address).floor();
        let hinted_end = hinted_start
            .as_usize()
            .checked_add(page_count)
            .map(VirtualPageNumber::from_vpn)
            .ok_or(MemoryError::InvalidRange)?;
        let user_end = (1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1)) / config::PAGE_SIZE;
        let hint_is_valid =
            address != 0 && hinted_start.as_usize() < user_end && hinted_end.as_usize() <= user_end;
        let range = if hint_is_valid && self.range_is_free(hinted_start, hinted_end) {
            hinted_start..hinted_end
        } else if fixed_noreplace {
            return Err(if hint_is_valid {
                MemoryError::AddressInUse
            } else {
                MemoryError::InvalidRange
            });
        } else {
            self.find_free_user_range(VirtualAddress::from(Self::MMAP_BASE).floor(), page_count)
                .ok_or(MemoryError::OutOfMemory)?
        };
        let start = usize::from(VirtualAddress::from(range.start));
        let end = usize::from(VirtualAddress::from(range.end));
        self.push(
            MapArea::file(start.into(), end.into(), permission),
            Some(data),
        )?;
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after file mmap page-table update");
        Ok(start)
    }

    fn overlapping_mmap_keys(
        &self,
        start: VirtualPageNumber,
        end: VirtualPageNumber,
    ) -> Result<Vec<VirtualPageNumber>, MemoryError> {
        let mut keys = Vec::new();
        for (key, area) in &self.areas {
            if start < area.vpn_range.end && area.vpn_range.start < end {
                if !matches!(area.kind, VmaKind::Anonymous | VmaKind::File) {
                    return Err(MemoryError::PermissionDenied);
                }
                keys.push(*key);
            }
        }
        Ok(keys)
    }

    fn merge_adjacent_anonymous(&mut self) {
        loop {
            let keys: Vec<_> = self.areas.keys().copied().collect();
            let Some((left_key, right_key)) = keys.windows(2).find_map(|pair| {
                let left = &self.areas[&pair[0]];
                let right = &self.areas[&pair[1]];
                (left.kind == VmaKind::Anonymous
                    && right.kind == VmaKind::Anonymous
                    && left.vpn_range.end == right.vpn_range.start
                    && left.map_permission == right.map_permission)
                    .then_some((pair[0], pair[1]))
            }) else {
                break;
            };
            let left = self.areas.remove(&left_key).unwrap();
            let right = self.areas.remove(&right_key).unwrap();
            self.areas.insert(left_key, left.merge_anonymous(right));
        }
    }

    /// @description 解除 anonymous 或 file-backed private 页；未映射洞按 Linux 语义忽略。
    ///
    /// @param address page-aligned 起始地址。
    /// @param length 非零字节长度，向上取整到整页。
    /// @return 成功返回空值；若触及非 anonymous VMA 则保持全部映射不变并拒绝。
    pub(crate) fn unmap_user_mapping(
        &mut self,
        address: usize,
        length: usize,
    ) -> Result<(), MemoryError> {
        let range = Self::checked_page_range(address, length)?;
        let keys = self.overlapping_mmap_keys(range.start, range.end)?;
        for key in keys {
            let area = self.areas.remove(&key).unwrap();
            let cut_start = range.start.max(area.vpn_range.start);
            let cut_end = range.end.min(area.vpn_range.end);
            let (left, mut middle, right) = area.partition_protectable(cut_start, cut_end);
            middle.unmap(&mut self.page_table);
            if let Some(left) = left {
                self.areas.insert(left.vpn_range.start, left);
            }
            if let Some(right) = right {
                self.areas.insert(right.vpn_range.start, right);
            }
        }
        if !self.range_is_free(range.start, range.end) {
            return Err(MemoryError::PermissionDenied);
        }
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after munmap page-table update");
        Ok(())
    }

    fn checked_page_range(
        address: usize,
        length: usize,
    ) -> Result<Range<VirtualPageNumber>, MemoryError> {
        if address == 0 || length == 0 || !VirtualAddress::from(address).is_aligned() {
            return Err(MemoryError::InvalidRange);
        }
        let end = address
            .checked_add(length)
            .and_then(|value| value.checked_add(config::PAGE_SIZE - 1))
            .map(|value| value / config::PAGE_SIZE * config::PAGE_SIZE)
            .ok_or(MemoryError::InvalidRange)?;
        let user_end = 1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1);
        if end > user_end {
            return Err(MemoryError::InvalidRange);
        }
        Ok(VirtualAddress::from(address).floor()..VirtualAddress::from(end).floor())
    }

    /// @description 修改完整 anonymous 或 ELF private 区间权限，并按边界拆分 VMA。
    ///
    /// @param address page-aligned 起始地址。
    /// @param length 非零字节长度，向上取整到整页。
    /// @param permission 新用户权限；允许 PROT_NONE，禁止 W+X。
    /// @return 成功返回空值；缺页或触及其他系统 VMA 时在修改前整体失败。
    pub(crate) fn protect_user_mapping(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
    ) -> Result<(), MemoryError> {
        if !permission.contains(MapPermission::U)
            || permission.contains(MapPermission::W | MapPermission::X)
        {
            return Err(MemoryError::InvalidRange);
        }
        let range = Self::checked_page_range(address, length)?;
        let mut keys = Vec::new();
        for (key, area) in &self.areas {
            if range.start < area.vpn_range.end && area.vpn_range.start < range.end {
                if !matches!(area.kind, VmaKind::Anonymous | VmaKind::Elf | VmaKind::File) {
                    return Err(MemoryError::PermissionDenied);
                }
                keys.push(*key);
            }
        }
        let mut covered = range.start;
        for key in &keys {
            let area = &self.areas[key];
            if area.vpn_range.start > covered {
                return Err(MemoryError::InvalidRange);
            }
            covered = covered.max(area.vpn_range.end);
        }
        if covered < range.end {
            return Err(MemoryError::InvalidRange);
        }
        for key in keys {
            let area = self.areas.remove(&key).unwrap();
            let change_start = range.start.max(area.vpn_range.start);
            let change_end = range.end.min(area.vpn_range.end);
            let (left, mut middle, right) = area.partition_protectable(change_start, change_end);
            let pte_flags = PTEFlags::from_bits(permission.bits()).unwrap();
            let old_has_leaf = MapArea::has_leaf_permission(middle.map_permission);
            let new_has_leaf = MapArea::has_leaf_permission(permission);
            for vpn in change_start.as_usize()..change_end.as_usize() {
                let vpn = VirtualPageNumber::from_vpn(vpn);
                match (old_has_leaf, new_has_leaf) {
                    (true, true) => self
                        .page_table
                        .set_flags(vpn, pte_flags)
                        .expect("accessible anonymous VMA must own a leaf PTE"),
                    (true, false) => self
                        .page_table
                        .unmap(vpn)
                        .expect("accessible anonymous VMA must own a leaf PTE"),
                    (false, true) => {
                        let ppn = middle
                            .data_frames
                            .get(&vpn)
                            .expect("anonymous VMA must own every reserved frame")
                            .ppn;
                        self.page_table
                            .map(vpn, ppn, pte_flags)
                            .expect("PROT_NONE VMA must own an empty reserved leaf slot");
                    }
                    (false, false) => {}
                }
            }
            middle.map_permission = permission;
            for segment in [left, Some(middle), right].into_iter().flatten() {
                self.areas.insert(segment.vpn_range.start, segment);
            }
        }
        self.merge_adjacent_anonymous();
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after mprotect page-table update");
        Ok(())
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

    fn map_elf_image(
        &mut self,
        elf_data: &[u8],
        load_bias: usize,
        allowed_type: xmas_elf::header::Type,
        allow_interpreter: bool,
    ) -> Result<LoadedElf, ElfLoadError> {
        const ELF64_PHDR_SIZE: usize = 56;
        let elf = xmas_elf::ElfFile::new(elf_data).map_err(|_| ElfLoadError::InvalidElf)?;
        let header = elf.header;
        if header.pt1.class() != xmas_elf::header::Class::SixtyFour
            || header.pt1.data() != xmas_elf::header::Data::LittleEndian
            || header.pt1.version() != xmas_elf::header::Version::Current
            || header.pt2.machine().as_machine() != xmas_elf::header::Machine::RISC_V
            || header.pt2.version() != 1
            || usize::from(header.pt2.header_size()) != 64
            || header.pt2.type_().as_type() != allowed_type
        {
            return Err(ElfLoadError::InvalidElf);
        }
        let flags = match header.pt2 {
            xmas_elf::header::HeaderPt2::Header64(value) => value.flags,
            xmas_elf::header::HeaderPt2::Header32(_) => return Err(ElfLoadError::InvalidElf),
        };
        if flags & !0x7 != 0 || flags & 0x6 == 0x6 {
            return Err(ElfLoadError::InvalidElf);
        }
        let ph_offset =
            usize::try_from(header.pt2.ph_offset()).map_err(|_| ElfLoadError::InvalidElf)?;
        let phent = usize::from(header.pt2.ph_entry_size());
        let phnum = usize::from(header.pt2.ph_count());
        let ph_end = ph_offset
            .checked_add(phent.checked_mul(phnum).ok_or(ElfLoadError::InvalidElf)?)
            .filter(|end| {
                phnum != 0 && ph_offset >= 64 && phent == ELF64_PHDR_SIZE && *end <= elf_data.len()
            })
            .ok_or(ElfLoadError::InvalidElf)?;
        let mut max_end = 0usize;
        let mut phdr = None;
        let mut loads = 0usize;
        for index in 0..header.pt2.ph_count() {
            let ph = elf
                .program_header(index)
                .map_err(|_| ElfLoadError::InvalidElf)?;
            match ph.get_type().map_err(|_| ElfLoadError::InvalidElf)? {
                xmas_elf::program::Type::Load => {
                    if ph.file_size() > ph.mem_size() {
                        return Err(ElfLoadError::InvalidElf);
                    }
                    let virtual_start =
                        usize::try_from(ph.virtual_addr()).map_err(|_| ElfLoadError::InvalidElf)?;
                    let start = load_bias
                        .checked_add(virtual_start)
                        .ok_or(ElfLoadError::InvalidElf)?;
                    let mem_size =
                        usize::try_from(ph.mem_size()).map_err(|_| ElfLoadError::InvalidElf)?;
                    let end = start
                        .checked_add(mem_size)
                        .ok_or(ElfLoadError::InvalidElf)?;
                    let file_start =
                        usize::try_from(ph.offset()).map_err(|_| ElfLoadError::InvalidElf)?;
                    let file_size =
                        usize::try_from(ph.file_size()).map_err(|_| ElfLoadError::InvalidElf)?;
                    let file_end = file_start
                        .checked_add(file_size)
                        .filter(|end| *end <= elf_data.len())
                        .ok_or(ElfLoadError::InvalidElf)?;
                    if mem_size == 0 {
                        if file_size != 0 {
                            return Err(ElfLoadError::InvalidElf);
                        }
                        continue;
                    }
                    let alignment =
                        usize::try_from(ph.align()).map_err(|_| ElfLoadError::InvalidElf)?;
                    if alignment > 1
                        && (!alignment.is_power_of_two()
                            || virtual_start % alignment != file_start % alignment)
                    {
                        return Err(ElfLoadError::InvalidElf);
                    }
                    let user_end = 1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1);
                    if start == 0 || start >= end || end > user_end {
                        return Err(ElfLoadError::InvalidElf);
                    }
                    let mut permission = MapPermission::U;
                    if ph.flags().is_read() {
                        permission |= MapPermission::R;
                    }
                    if ph.flags().is_write() {
                        permission |= MapPermission::W;
                    }
                    if ph.flags().is_execute() {
                        permission |= MapPermission::X;
                    }
                    if permission.contains(MapPermission::W | MapPermission::X) {
                        return Err(ElfLoadError::InvalidElf);
                    }
                    self.push(
                        MapArea::elf(start.into(), end.into(), permission),
                        Some(&elf_data[file_start..file_end]),
                    )
                    .map_err(ElfLoadError::from)?;
                    max_end = max_end.max(end);
                    loads += 1;
                    if file_start <= ph_offset && ph_end <= file_end {
                        phdr = start.checked_add(ph_offset - file_start);
                    }
                }
                xmas_elf::program::Type::Interp if allow_interpreter => {}
                xmas_elf::program::Type::Dynamic | xmas_elf::program::Type::Tls => {}
                xmas_elf::program::Type::Interp => return Err(ElfLoadError::InvalidElf),
                xmas_elf::program::Type::OsSpecific(0x6474_e551) if ph.flags().is_execute() => {
                    return Err(ElfLoadError::InvalidElf);
                }
                _ => {}
            }
        }
        if loads == 0 {
            return Err(ElfLoadError::InvalidElf);
        }
        let entry = load_bias
            .checked_add(
                usize::try_from(header.pt2.entry_point()).map_err(|_| ElfLoadError::InvalidElf)?,
            )
            .ok_or(ElfLoadError::InvalidElf)?;
        let entry_pte = self
            .translate(VirtualAddress::from(entry).floor())
            .ok_or(ElfLoadError::InvalidElf)?;
        if !entry_pte.flags().contains(PTEFlags::U | PTEFlags::X) {
            return Err(ElfLoadError::InvalidElf);
        }
        Ok(LoadedElf {
            entry,
            phdr,
            phent,
            phnum,
            max_end,
        })
    }

    /// @description 从 RV64 ET_EXEC 或动态 PIE+PT_INTERP 构造用户地址空间和 Linux 初始栈。
    ///
    /// @param elf_data 完整 ELF file bytes。
    /// @param args 不含 NUL 的 argv 字节串。
    /// @param envs 不含 NUL 的 envp 字节串。
    /// @return 新 MemorySet、16-byte aligned 用户 sp 与 ELF entry。
    /// @errors 只区分资源耗尽与非法/不支持的 ELF，且失败时不修改现有地址空间。
    pub(crate) fn from_elf(
        image: &ExecutableImage,
        args: &[Vec<u8>],
        envs: &[Vec<u8>],
    ) -> Result<(Self, usize, usize), ElfLoadError> {
        let mut memory_set = MemorySet::try_new().map_err(ElfLoadError::from)?;
        memory_set.map_trampoline().map_err(ElfLoadError::from)?;
        const MAIN_PIE_BASE: usize = 0x1_0000;
        const INTERPRETER_BASE: usize = 0x2000_0000;
        let main_type = xmas_elf::ElfFile::new(&image.main)
            .map_err(|_| ElfLoadError::InvalidElf)?
            .header
            .pt2
            .type_()
            .as_type();
        let main_bias = match main_type {
            xmas_elf::header::Type::Executable if image.interpreter.is_none() => 0,
            xmas_elf::header::Type::SharedObject if image.interpreter.is_some() => MAIN_PIE_BASE,
            _ => return Err(ElfLoadError::InvalidElf),
        };
        let main = memory_set.map_elf_image(&image.main, main_bias, main_type, true)?;
        let (entry_point, interpreter_base) = if let Some(interpreter) = &image.interpreter {
            let loaded = memory_set.map_elf_image(
                interpreter,
                INTERPRETER_BASE,
                xmas_elf::header::Type::SharedObject,
                false,
            )?;
            (loaded.entry, INTERPRETER_BASE)
        } else {
            (main.entry, 0)
        };
        let phdr_address = main.phdr.ok_or(ElfLoadError::InvalidElf)?;
        let heap_base = VirtualAddress::from(main.max_end).ceil().as_usize() * config::PAGE_SIZE;
        let user_end = 1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1);
        let user_stack_top = user_end
            .checked_sub(config::PAGE_SIZE)
            .ok_or(ElfLoadError::InvalidElf)?;
        let user_stack_bottom = user_stack_top
            .checked_sub(config::USER_STACK_SIZE)
            .ok_or(ElfLoadError::InvalidElf)?;
        let heap_limit = user_stack_bottom
            .checked_sub(config::PAGE_SIZE)
            .ok_or(ElfLoadError::InvalidElf)?;
        if heap_base >= heap_limit {
            return Err(ElfLoadError::InvalidElf);
        }

        // 2. heap 从最高 LOAD 末端开始；栈位于 Sv39 低半区顶部，上下各保留一页 guard。
        memory_set
            .push(
                MapArea::new(
                    user_stack_bottom.into(),
                    user_stack_top.into(),
                    MapType::Framed,
                    MapPermission::R | MapPermission::W | MapPermission::U,
                ),
                None,
            )
            .map_err(ElfLoadError::from)?;
        memory_set
            .initialize_user_heap(heap_base, heap_limit)
            .map_err(ElfLoadError::from)?;
        memory_set
            .push(
                MapArea::new(
                    config::TRAP_CONTEXT.into(),
                    config::TRAMPOLINE.into(),
                    MapType::Framed,
                    MapPermission::R | MapPermission::W,
                ),
                None,
            )
            .map_err(ElfLoadError::from)?;

        let phdr_pte = memory_set
            .translate(VirtualAddress::from(phdr_address).floor())
            .ok_or(ElfLoadError::InvalidElf)?;
        if !phdr_pte.flags().contains(PTEFlags::U | PTEFlags::R) {
            return Err(ElfLoadError::InvalidElf);
        }

        // 3. 初始栈是 argv/envp/auxv 的唯一用户契约，不通过寄存器传递私有参数。
        let aux = ElfAuxInfo {
            phdr: phdr_address,
            phent: main.phent,
            phnum: main.phnum,
            entry: main.entry,
            base: interpreter_base,
        };
        let actual_stack_top = memory_set.build_initial_stack(user_stack_top, args, envs, aux)?;
        Ok((memory_set, actual_stack_top, entry_point))
    }

    fn build_initial_stack(
        &mut self,
        stack_top: usize,
        args: &[Vec<u8>],
        envs: &[Vec<u8>],
        aux: ElfAuxInfo,
    ) -> Result<usize, ElfLoadError> {
        const AT_NULL: usize = 0;
        const AT_PHDR: usize = 3;
        const AT_PHENT: usize = 4;
        const AT_PHNUM: usize = 5;
        const AT_PAGESZ: usize = 6;
        const AT_BASE: usize = 7;
        const AT_ENTRY: usize = 9;
        const AT_RANDOM: usize = 25;
        const AT_EXECFN: usize = 31;
        const AUX_WORDS: usize = 18;
        const RANDOM_BYTES: usize = 16;

        let total_string_size = args
            .iter()
            .chain(envs)
            .try_fold(0usize, |total, value| {
                value
                    .len()
                    .checked_add(1)
                    .and_then(|size| total.checked_add(size))
            })
            .and_then(|size| size.checked_add(RANDOM_BYTES))
            .ok_or(ElfLoadError::InvalidElf)?;
        let pointer_count = 1usize
            .checked_add(args.len())
            .and_then(|count| count.checked_add(1))
            .and_then(|count| count.checked_add(envs.len()))
            .and_then(|count| count.checked_add(1))
            .and_then(|count| count.checked_add(AUX_WORDS))
            .ok_or(ElfLoadError::InvalidElf)?;
        let pointer_space = pointer_count
            .checked_mul(core::mem::size_of::<usize>())
            .ok_or(ElfLoadError::InvalidElf)?;
        let unaligned_size = pointer_space
            .checked_add(total_string_size)
            .ok_or(ElfLoadError::InvalidElf)?;
        let stack_size = unaligned_size
            .checked_add(15)
            .ok_or(ElfLoadError::InvalidElf)?;
        if stack_size > config::USER_STACK_SIZE {
            return Err(ElfLoadError::InvalidElf);
        }
        let stack_ptr = stack_top
            .checked_sub(stack_size)
            .ok_or(ElfLoadError::InvalidElf)?
            & !15usize;
        let mut string_ptr = stack_ptr
            .checked_add(pointer_space)
            .ok_or(ElfLoadError::InvalidElf)?;
        let mut argv_ptrs = Vec::new();
        argv_ptrs
            .try_reserve_exact(args.len())
            .map_err(|_| ElfLoadError::OutOfMemory)?;
        let mut envp_ptrs = Vec::new();
        envp_ptrs
            .try_reserve_exact(envs.len())
            .map_err(|_| ElfLoadError::OutOfMemory)?;

        for arg in args {
            argv_ptrs.push(string_ptr);
            self.write_c_string_to_user_stack(string_ptr, arg)?;
            string_ptr = string_ptr
                .checked_add(arg.len())
                .and_then(|address| address.checked_add(1))
                .ok_or(ElfLoadError::InvalidElf)?;
        }
        for env in envs {
            envp_ptrs.push(string_ptr);
            self.write_c_string_to_user_stack(string_ptr, env)?;
            string_ptr = string_ptr
                .checked_add(env.len())
                .and_then(|address| address.checked_add(1))
                .ok_or(ElfLoadError::InvalidElf)?;
        }
        let random_ptr = string_ptr;
        let mut random = [0u8; RANDOM_BYTES];
        crate::random::fill(&mut random).map_err(|_| ElfLoadError::InvalidElf)?;
        self.copy_to_user(random_ptr, &random)
            .map_err(|_| ElfLoadError::InvalidElf)?;
        let execfn = argv_ptrs.first().copied().unwrap_or(0);

        let mut writer = stack_ptr;
        self.write_usize_to_user_stack(writer, args.len())?;
        writer += core::mem::size_of::<usize>();
        for pointer in argv_ptrs {
            self.write_usize_to_user_stack(writer, pointer)?;
            writer += core::mem::size_of::<usize>();
        }
        self.write_usize_to_user_stack(writer, 0)?;
        writer += core::mem::size_of::<usize>();
        for pointer in envp_ptrs {
            self.write_usize_to_user_stack(writer, pointer)?;
            writer += core::mem::size_of::<usize>();
        }
        self.write_usize_to_user_stack(writer, 0)?;
        writer += core::mem::size_of::<usize>();

        for (kind, value) in [
            (AT_PHDR, aux.phdr),
            (AT_PHENT, aux.phent),
            (AT_PHNUM, aux.phnum),
            (AT_PAGESZ, config::PAGE_SIZE),
            (AT_BASE, aux.base),
            (AT_ENTRY, aux.entry),
            (AT_RANDOM, random_ptr),
            (AT_EXECFN, execfn),
            (AT_NULL, 0),
        ] {
            self.write_usize_to_user_stack(writer, kind)?;
            writer += core::mem::size_of::<usize>();
            self.write_usize_to_user_stack(writer, value)?;
            writer += core::mem::size_of::<usize>();
        }

        debug_assert_eq!(writer, stack_ptr + pointer_space);
        debug_assert_eq!(stack_ptr & 15, 0);
        Ok(stack_ptr)
    }

    fn write_c_string_to_user_stack(
        &mut self,
        address: usize,
        value: &[u8],
    ) -> Result<(), ElfLoadError> {
        self.copy_to_user(address, value)
            .map_err(|_| ElfLoadError::InvalidElf)?;
        let nul_address = address
            .checked_add(value.len())
            .ok_or(ElfLoadError::InvalidElf)?;
        self.copy_to_user(nul_address, &[0])
            .map_err(|_| ElfLoadError::InvalidElf)
    }

    fn write_usize_to_user_stack(
        &mut self,
        address: usize,
        value: usize,
    ) -> Result<(), ElfLoadError> {
        self.copy_to_user(address, &value.to_le_bytes())
            .map_err(|_| ElfLoadError::InvalidElf)
    }
}

#[derive(Debug, Clone, Copy)]
struct ElfAuxInfo {
    phdr: usize,
    phent: usize,
    phnum: usize,
    entry: usize,
    base: usize,
}

#[derive(Debug, Clone, Copy)]
struct LoadedElf {
    entry: usize,
    phdr: Option<usize>,
    phent: usize,
    phnum: usize,
    max_end: usize,
}
