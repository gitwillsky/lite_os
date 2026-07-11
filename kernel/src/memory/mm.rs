use core::{arch::asm, error::Error, ops::Range};

use alloc::{boxed::Box, collections::BTreeMap, string::String, vec::Vec};
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
pub enum MemoryError {
    OutOfMemory,
    PageTableError(PageTableError),
    InvalidRange,
}

/// @description 用户地址复制失败原因；所有成员都表示不能完成完整 copyin/copyout。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAccessError {
    /// 地址为空、非用户 canonical 地址、未映射或权限不匹配。
    Fault,
    /// 地址加长度发生整数溢出。
    Overflow,
    /// 在调用方指定上限内没有找到 NUL。
    Unterminated,
    /// 字节串不是当前 VFS 接口可接受的 UTF-8。
    InvalidUtf8,
}

impl core::fmt::Display for UserAccessError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Fault => write!(f, "invalid user address or permission"),
            Self::Overflow => write!(f, "user address range overflow"),
            Self::Unterminated => write!(f, "unterminated user string"),
            Self::InvalidUtf8 => write!(f, "invalid UTF-8 user string"),
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
            MemoryError::PageTableError(err) => write!(f, "Page table error: {}", err),
            MemoryError::InvalidRange => write!(f, "Invalid virtual memory range"),
        }
    }
}

impl Error for MemoryError {}

bitflags! {
    // PTE Flags 的子集
    #[derive(Debug, Clone, Copy)]
    pub struct MapPermission: u8 {
        const R = 1 << 1; // 可读
        const W = 1 << 2; // 可写
        const X = 1 << 3; // 可执行
        const U = 1 << 4; // 用户态可访问 (默认仅 内核 态可访问)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum MapType {
    Identical, // PA <-> VA 恒等映射
    Framed,    // 映射到分配的物理页帧
}

#[derive(Debug)]
pub struct MapArea {
    vpn_range: Range<VirtualPageNumber>,
    data_page_offset: usize,
    data_frames: BTreeMap<VirtualPageNumber, FrameTracker>,
    map_type: MapType,
    map_permission: MapPermission,
    /// 是否标记为全局页（G位）。仅用于内核空间映射。
    global: bool,
}

impl MapArea {
    pub fn new(
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
        }
    }

    pub fn set_global(mut self, global: bool) -> Self {
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

    pub fn map(&mut self, page_table: &mut PageTable) -> Result<(), MemoryError> {
        for vpn in self.vpn_range.start.as_usize()..self.vpn_range.end.as_usize() {
            self.map_one(page_table, VirtualPageNumber::from_vpn(vpn))?;
        }
        Ok(())
    }

    pub fn unmap(&mut self, page_table: &mut PageTable) {
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

        let mut pte_flags = PTEFlags::from_bits(self.map_permission.bits()).unwrap();
        if self.global {
            pte_flags |= PTEFlags::G;
        }
        page_table.map(vpn, ppn, pte_flags)?;
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

    pub fn shrink_to(
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

    pub fn append_to(
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
}

#[derive(Debug)]
pub struct MemorySet {
    page_table: PageTable,
    areas: Vec<MapArea>,
    user_heap: Option<UserHeap>,
}

#[derive(Debug, Clone)]
struct UserHeap {
    range: Range<usize>,
    limit: usize,
}

impl MemorySet {
    pub fn new() -> Self {
        Self {
            page_table: PageTable::new(),
            areas: Vec::new(),
            user_heap: None,
        }
    }

    fn try_new() -> Result<Self, MemoryError> {
        Ok(Self {
            page_table: PageTable::try_new()?,
            areas: Vec::new(),
            user_heap: None,
        })
    }

    pub fn push(&mut self, mut map_area: MapArea, data: Option<&[u8]>) -> Result<(), MemoryError> {
        // 先尝试映射；若中途失败，需要回滚已映射的页面，避免留下半映射导致后续查找空闲区域极慢
        if let Err(e) = map_area.map(&mut self.page_table) {
            // 回滚：解除已经映射的页面
            map_area.unmap(&mut self.page_table);
            return Err(e);
        }
        if let Some(data) = data {
            if let Err(error) = map_area.copy_data(data) {
                map_area.unmap(&mut self.page_table);
                return Err(error);
            }
        }
        self.areas.push(map_area);
        Ok(())
    }

    pub fn insert_framed_area(
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

    pub fn token(&self) -> usize {
        self.page_table.token()
    }

    pub fn map_trampoline(&mut self) -> Result<(), MemoryError> {
        let trampoline_va = VirtualAddress::from(config::TRAMPOLINE);
        let strampoline_pa = PhysicalAddress::from(strampoline as usize);

        self.page_table.map(
            trampoline_va.into(),
            strampoline_pa.into(),
            // Trampoline 在所有地址空间通用，标记为 Global，避免跨进程切换时TLB混淆
            PTEFlags::R | PTEFlags::X | PTEFlags::G,
        )?;
        Ok(())
    }

    pub fn active(&self) {
        let satp = self.page_table.token();
        unsafe {
            satp::write(Satp::from_bits(satp));
            asm!("sfence.vma")
        }
    }

    /// @description 同步刷新所有 online hart 的 S-stage TLB。
    ///
    /// @return 所有目标 hart 完成 `SFENCE.VMA` 后返回 `Ok(())`；SBI RFENCE 失败时返回错误码。
    pub fn flush_tlb_all_cpus() -> Result<(), isize> {
        // 1. 本 hart 先完成 fence；当前页表写在后续 SBI ecall 之前保持程序顺序。
        unsafe { asm!("sfence.vma") }
        // 2. Acquire online mask 只选择已发布可接收远端请求的 hart。
        let current = crate::arch::hart::hart_id();
        let targets = crate::arch::hart::online_hart_mask() & !(1usize << current);
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
    pub fn translate_kernel_address(
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
    pub fn copy_from_user(
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
    pub fn copy_to_user(
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

    /// @description 从用户空间复制有长度上限的 NUL 结尾 UTF-8 字符串。
    ///
    /// @param user_address 字符串首地址。
    /// @param max_len 不含 NUL 的最大字节数。
    /// @return 成功返回 owned `String`；fault、未终止或非法 UTF-8 分别返回明确错误。
    pub fn copy_user_string(
        &self,
        user_address: usize,
        max_len: usize,
    ) -> Result<String, UserAccessError> {
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
                bytes.extend_from_slice(&page[..nul]);
                return String::from_utf8(bytes).map_err(|_| UserAccessError::InvalidUtf8);
            }
            bytes.extend_from_slice(page);
            current = current
                .checked_add(count)
                .ok_or(UserAccessError::Overflow)?;
        }
        Err(UserAccessError::Unterminated)
    }

    /// @description 在不修改用户内存的情况下检查完整 `U|W` 范围。
    ///
    /// @param user_address 用户目标地址。
    /// @param len 待写长度。
    /// @return 完整范围可写返回 `true`；overflow、缺页或权限错误返回 `false`。
    pub fn is_user_writable(&self, user_address: usize, len: usize) -> bool {
        self.validate_user_range(user_address, len, PTEFlags::W)
            .is_ok()
    }

    fn initialize_user_heap(&mut self, base: usize, limit: usize) -> Result<(), MemoryError> {
        if base >= limit || !VirtualAddress::from(base).is_aligned() {
            return Err(MemoryError::InvalidRange);
        }
        self.push(
            MapArea::new(
                base.into(),
                base.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        )?;
        self.user_heap = Some(UserHeap {
            range: base..base,
            limit,
        });
        Ok(())
    }

    /// @description 查询或原子提交当前用户地址空间的 program break。
    ///
    /// @param new_break 新 break；零表示只查询。
    /// @return 成功返回提交后的 break；越界、映射冲突或 OOM 时返回错误且保持旧 break。
    pub fn set_program_break(&mut self, new_break: usize) -> Result<usize, MemoryError> {
        let heap = self.user_heap.clone().ok_or(MemoryError::InvalidRange)?;
        if new_break == 0 {
            return Ok(heap.range.end);
        }
        if new_break < heap.range.start || new_break > heap.limit {
            return Err(MemoryError::InvalidRange);
        }

        let area = self
            .areas
            .iter_mut()
            .find(|area| area.vpn_range.start == VirtualAddress::from(heap.range.start).floor())
            .ok_or(MemoryError::InvalidRange)?;
        let old_page_end = VirtualAddress::from(heap.range.end).ceil();
        let new_page_end = VirtualAddress::from(new_break).ceil();
        if new_page_end > old_page_end {
            area.append_to(&mut self.page_table, new_page_end)?;
        } else if new_page_end < old_page_end {
            area.shrink_to(&mut self.page_table, new_page_end)?;
        }
        self.user_heap.as_mut().unwrap().range.end = new_break;

        if new_page_end != old_page_end {
            Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after brk page-table update");
        }
        Ok(new_break)
    }

    pub fn remove_area_with_start_vpn(&mut self, start_vpn: VirtualPageNumber) {
        if let Some(idx) = self
            .areas
            .iter()
            .position(|area| area.vpn_range.start == start_vpn)
        {
            // 将目标区域移出容器后再执行 unmap，规避潜在别名问题
            let mut area = self.areas.remove(idx);
            area.unmap(&mut self.page_table);
        }
    }

    /// 获取给定TrapContext虚拟地址的物理页号
    pub fn trap_context_ppn(&self, trap_va: usize) -> PhysicalPageNumber {
        self.page_table
            .translate(VirtualAddress::from(trap_va).into())
            .expect("TrapContext VA should be mapped")
            .ppn()
    }

    pub fn from_elf(elf_data: &[u8]) -> Result<(Self, usize, usize), Box<dyn Error>> {
        Self::from_elf_internal(elf_data, &[], &[])
    }

    fn from_elf_internal(
        elf_data: &[u8],
        args: &[String],
        envs: &[String],
    ) -> Result<(Self, usize, usize), Box<dyn Error>> {
        let mut memory_set = MemorySet::try_new()?;
        memory_set.map_trampoline()?;

        let elf = xmas_elf::ElfFile::new(elf_data)?;
        let elf_header = elf.header;
        if elf_header.pt1.class() != xmas_elf::header::Class::SixtyFour
            || elf_header.pt1.data() != xmas_elf::header::Data::LittleEndian
            || elf_header.pt2.machine().as_machine() != xmas_elf::header::Machine::RISC_V
            || elf_header.pt2.type_().as_type() != xmas_elf::header::Type::Executable
        {
            return Err("unsupported ELF class, endian, machine, or type".into());
        }

        let ph_count = elf_header.pt2.ph_count();
        let mut max_mapped_vpn = VirtualPageNumber::from(0);
        let mut load_segments = 0usize;

        // 1. 每个 LOAD segment 先完成 checked bounds 与权限验证，再修改地址空间。
        for i in 0..ph_count {
            let ph = elf.program_header(i)?;
            match ph.get_type()? {
                xmas_elf::program::Type::Load => {
                    if ph.file_size() > ph.mem_size() {
                        return Err("ELF LOAD file size exceeds memory size".into());
                    }
                    let start = usize::try_from(ph.virtual_addr())?;
                    let mem_size = usize::try_from(ph.mem_size())?;
                    let end = start
                        .checked_add(mem_size)
                        .ok_or("ELF LOAD virtual range overflow")?;
                    let file_start = usize::try_from(ph.offset())?;
                    let file_size = usize::try_from(ph.file_size())?;
                    let file_end = file_start
                        .checked_add(file_size)
                        .ok_or("ELF LOAD file range overflow")?;
                    if mem_size == 0 {
                        if file_size != 0 {
                            return Err("zero-sized ELF LOAD contains file bytes".into());
                        }
                        continue;
                    }
                    let alignment = usize::try_from(ph.align())?;
                    if alignment > 1
                        && (!alignment.is_power_of_two()
                            || start % alignment != file_start % alignment)
                    {
                        return Err("invalid ELF LOAD alignment".into());
                    }
                    let user_end = 1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1);
                    if start == 0 || start >= end || end > user_end || file_end > elf_data.len() {
                        return Err("invalid ELF LOAD range".into());
                    }
                    load_segments += 1;

                    let mut map_perm = MapPermission::U;
                    let ph_flags = ph.flags();
                    if ph_flags.is_execute() {
                        map_perm |= MapPermission::X
                    }
                    if ph_flags.is_read() {
                        map_perm |= MapPermission::R
                    }
                    if ph_flags.is_write() {
                        map_perm |= MapPermission::W
                    }
                    if map_perm.contains(MapPermission::W | MapPermission::X) {
                        return Err("writable executable ELF segment is forbidden".into());
                    }
                    let map_area =
                        MapArea::new(start.into(), end.into(), MapType::Framed, map_perm);

                    max_mapped_vpn = max_mapped_vpn
                        .as_usize()
                        .max(map_area.vpn_range.end.as_usize())
                        .into();
                    memory_set.push(map_area, Some(&elf_data[file_start..file_end]))?;
                }
                xmas_elf::program::Type::Dynamic | xmas_elf::program::Type::Interp => {
                    return Err("dynamic ELF is not supported".into());
                }
                _ => {}
            }
        }
        if load_segments == 0 {
            return Err("ELF has no LOAD segment".into());
        }

        let max_end_va: VirtualAddress = max_mapped_vpn.into();
        let heap_base = usize::from(max_end_va);
        let user_end = 1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1);
        let user_stack_top = user_end
            .checked_sub(config::PAGE_SIZE)
            .ok_or("user stack top underflow")?;
        let user_stack_bottom = user_stack_top
            .checked_sub(config::USER_STACK_SIZE)
            .ok_or("user stack range underflow")?;
        let heap_limit = user_stack_bottom
            .checked_sub(config::PAGE_SIZE)
            .ok_or("user stack guard underflow")?;
        if heap_base >= heap_limit {
            return Err("ELF image overlaps user stack or guard".into());
        }

        // 2. heap 从最高 LOAD 末端开始；栈位于 Sv39 低半区顶部，栈上下各保留一页 guard。
        memory_set.push(
            MapArea::new(
                user_stack_bottom.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        )?;
        memory_set.initialize_user_heap(heap_base, heap_limit)?;

        memory_set.push(
            MapArea::new(
                config::TRAP_CONTEXT.into(),
                config::TRAMPOLINE.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W,
            ),
            None,
        )?;

        let entry_point = usize::try_from(elf.header.pt2.entry_point())?;
        let entry_pte = memory_set
            .translate(VirtualAddress::from(entry_point).floor())
            .ok_or("ELF entry is not mapped")?;
        if !entry_pte.flags().contains(PTEFlags::U | PTEFlags::X) {
            return Err("ELF entry is not user executable".into());
        }

        // 3. 参数栈是唯一 argv/envp 路径；即使为空也写入 argc 与两个终止指针。
        let actual_stack_top = memory_set.build_arg_stack(user_stack_top, args, envs)?;
        Ok((memory_set, actual_stack_top, entry_point))
    }

    /// Create a new memory set from ELF data with argument support
    pub fn from_elf_with_args(
        elf_data: &[u8],
        args: &[String],
        envs: &[String],
    ) -> Result<(Self, usize, usize), Box<dyn Error>> {
        Self::from_elf_internal(elf_data, args, envs)
    }

    /// Build argc/argv/envp layout on user stack
    fn build_arg_stack(
        &mut self,
        stack_top: usize,
        args: &[String],
        envs: &[String],
    ) -> Result<usize, Box<dyn Error>> {
        let total_string_size = args
            .iter()
            .chain(envs)
            .try_fold(0usize, |total, value| {
                value
                    .len()
                    .checked_add(1)
                    .and_then(|size| total.checked_add(size))
            })
            .ok_or("argument string bytes overflow")?;
        let argc = args.len();
        let pointer_count = 1usize
            .checked_add(argc)
            .and_then(|count| count.checked_add(1))
            .and_then(|count| count.checked_add(envs.len()))
            .and_then(|count| count.checked_add(1))
            .ok_or("argument pointer count overflow")?;
        let pointer_space = pointer_count
            .checked_mul(core::mem::size_of::<usize>())
            .ok_or("argument pointer bytes overflow")?;
        let total_size = pointer_space
            .checked_add(total_string_size)
            .and_then(|size| size.checked_add(15))
            .ok_or("argument stack size overflow")?;
        let stack_ptr = stack_top
            .checked_sub(total_size)
            .ok_or("argument stack underflow")?
            & !15usize;

        let string_area_start = stack_ptr
            .checked_add(pointer_space)
            .ok_or("argument string address overflow")?;
        let mut string_ptr = string_area_start;
        let mut argv_ptrs = Vec::new();
        let mut envp_ptrs = Vec::new();

        for arg in args {
            argv_ptrs.push(string_ptr);
            self.write_string_to_user_stack(string_ptr, arg)?;
            string_ptr = string_ptr
                .checked_add(arg.len())
                .and_then(|address| address.checked_add(1))
                .ok_or("argument string address overflow")?;
        }

        for env in envs {
            envp_ptrs.push(string_ptr);
            self.write_string_to_user_stack(string_ptr, env)?;
            string_ptr = string_ptr
                .checked_add(env.len())
                .and_then(|address| address.checked_add(1))
                .ok_or("environment string address overflow")?;
        }

        let mut ptr_writer = stack_ptr;
        self.write_usize_to_user_stack(ptr_writer, argc)?;
        ptr_writer += core::mem::size_of::<usize>();

        // Write argv pointers
        for &arg_ptr in &argv_ptrs {
            self.write_usize_to_user_stack(ptr_writer, arg_ptr)?;
            ptr_writer += core::mem::size_of::<usize>();
        }
        self.write_usize_to_user_stack(ptr_writer, 0)?;
        ptr_writer += core::mem::size_of::<usize>();

        // Write envp pointers
        for &env_ptr in &envp_ptrs {
            self.write_usize_to_user_stack(ptr_writer, env_ptr)?;
            ptr_writer += core::mem::size_of::<usize>();
        }
        self.write_usize_to_user_stack(ptr_writer, 0)?;

        Ok(stack_ptr)
    }

    fn write_string_to_user_stack(&mut self, addr: usize, s: &str) -> Result<(), Box<dyn Error>> {
        self.copy_to_user(addr, s.as_bytes())?;
        let nul_address = addr
            .checked_add(s.len())
            .ok_or("argument NUL address overflow")?;
        self.copy_to_user(nul_address, &[0])?;
        Ok(())
    }

    fn write_usize_to_user_stack(
        &mut self,
        addr: usize,
        value: usize,
    ) -> Result<(), Box<dyn Error>> {
        self.copy_to_user(addr, &value.to_le_bytes())?;
        Ok(())
    }
}
