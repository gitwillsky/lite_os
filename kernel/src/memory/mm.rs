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
            MemoryError::PageTableError(err) => write!(f, "Page table error: {}", err),
            MemoryError::InvalidRange => write!(f, "Invalid virtual memory range"),
        }
    }
}

impl Error for MemoryError {}

/// @description 构造新用户映像时需要暴露给 `execve` 的失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfLoadError {
    /// 物理页或页表页分配失败。
    OutOfMemory,
    /// ELF header、segment、地址、权限或初始栈不满足当前静态 RV64 契约。
    InvalidElf,
}

impl From<MemoryError> for ElfLoadError {
    fn from(error: MemoryError) -> Self {
        match error {
            MemoryError::OutOfMemory | MemoryError::PageTableError(PageTableError::OutOfMemory) => {
                Self::OutOfMemory
            }
            MemoryError::PageTableError(_) | MemoryError::InvalidRange => Self::InvalidElf,
        }
    }
}

impl core::fmt::Display for ElfLoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutOfMemory => write!(f, "out of memory while loading ELF"),
            Self::InvalidElf => write!(f, "invalid or unsupported static RV64 ELF"),
        }
    }
}

impl Error for ElfLoadError {}

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

    /// @description 从用户空间复制有长度上限的 NUL 结尾字节串。
    ///
    /// @param user_address 字符串首地址。
    /// @param max_len 包含终止 NUL 的最大总字节数。
    /// @return 成功返回不含 NUL 的 owned bytes；fault、未终止或内存不足返回明确错误。
    pub fn copy_user_c_string(
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

    /// @description 从静态 RV64 ELF 构造完整用户地址空间和 Linux 初始栈。
    ///
    /// @param elf_data 完整 ELF file bytes。
    /// @param args 不含 NUL 的 argv 字节串。
    /// @param envs 不含 NUL 的 envp 字节串。
    /// @return 新 MemorySet、16-byte aligned 用户 sp 与 ELF entry。
    /// @errors 只区分资源耗尽与非法/不支持的 ELF，且失败时不修改现有地址空间。
    pub fn from_elf(
        elf_data: &[u8],
        args: &[Vec<u8>],
        envs: &[Vec<u8>],
    ) -> Result<(Self, usize, usize), ElfLoadError> {
        const ELF64_PHDR_SIZE: usize = 56;

        let mut memory_set = MemorySet::try_new().map_err(ElfLoadError::from)?;
        memory_set.map_trampoline().map_err(ElfLoadError::from)?;

        let elf = xmas_elf::ElfFile::new(elf_data).map_err(|_| ElfLoadError::InvalidElf)?;
        let elf_header = elf.header;
        if elf_header.pt1.class() != xmas_elf::header::Class::SixtyFour
            || elf_header.pt1.data() != xmas_elf::header::Data::LittleEndian
            || elf_header.pt1.version() != xmas_elf::header::Version::Current
            || elf_header.pt2.machine().as_machine() != xmas_elf::header::Machine::RISC_V
            || elf_header.pt2.version() != 1
            || usize::from(elf_header.pt2.header_size()) != 64
            || elf_header.pt2.type_().as_type() != xmas_elf::header::Type::Executable
        {
            return Err(ElfLoadError::InvalidElf);
        }
        let elf_flags = match elf_header.pt2 {
            xmas_elf::header::HeaderPt2::Header64(header) => header.flags,
            xmas_elf::header::HeaderPt2::Header32(_) => return Err(ElfLoadError::InvalidElf),
        };
        // 只接受 RVC 与 soft/single/double-float ABI；RV32E、quad-float、TSO 或未知 flag 都缺少对应执行环境。
        if elf_flags & !0x7 != 0 || elf_flags & 0x6 == 0x6 {
            return Err(ElfLoadError::InvalidElf);
        }

        let ph_offset =
            usize::try_from(elf_header.pt2.ph_offset()).map_err(|_| ElfLoadError::InvalidElf)?;
        let ph_entry_size = usize::from(elf_header.pt2.ph_entry_size());
        let ph_count = usize::from(elf_header.pt2.ph_count());
        if ph_count == 0 || ph_offset < 64 || ph_entry_size != ELF64_PHDR_SIZE {
            return Err(ElfLoadError::InvalidElf);
        }
        let ph_size = ph_entry_size
            .checked_mul(ph_count)
            .ok_or(ElfLoadError::InvalidElf)?;
        let ph_end = ph_offset
            .checked_add(ph_size)
            .filter(|end| *end <= elf_data.len())
            .ok_or(ElfLoadError::InvalidElf)?;

        let mut max_mapped_vpn = VirtualPageNumber::from(0);
        let mut load_segments = 0usize;
        let mut phdr_address = None;

        // 1. 每个 LOAD 先完成 checked bounds、alignment 与 W^X 验证，再将它作为唯一映射 owner 提交。
        for index in 0..elf_header.pt2.ph_count() {
            let ph = elf
                .program_header(index)
                .map_err(|_| ElfLoadError::InvalidElf)?;
            match ph.get_type().map_err(|_| ElfLoadError::InvalidElf)? {
                xmas_elf::program::Type::Load => {
                    if ph.file_size() > ph.mem_size() {
                        return Err(ElfLoadError::InvalidElf);
                    }
                    let start =
                        usize::try_from(ph.virtual_addr()).map_err(|_| ElfLoadError::InvalidElf)?;
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
                            || start % alignment != file_start % alignment)
                    {
                        return Err(ElfLoadError::InvalidElf);
                    }
                    let user_end = 1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1);
                    if start == 0 || start >= end || end > user_end || file_end > elf_data.len() {
                        return Err(ElfLoadError::InvalidElf);
                    }

                    let mut map_perm = MapPermission::U;
                    let ph_flags = ph.flags();
                    if ph_flags.is_execute() {
                        map_perm |= MapPermission::X;
                    }
                    if ph_flags.is_read() {
                        map_perm |= MapPermission::R;
                    }
                    if ph_flags.is_write() {
                        map_perm |= MapPermission::W;
                    }
                    if map_perm.contains(MapPermission::W | MapPermission::X) {
                        return Err(ElfLoadError::InvalidElf);
                    }

                    let map_area =
                        MapArea::new(start.into(), end.into(), MapType::Framed, map_perm);
                    max_mapped_vpn = max_mapped_vpn
                        .as_usize()
                        .max(map_area.vpn_range.end.as_usize())
                        .into();
                    memory_set
                        .push(map_area, Some(&elf_data[file_start..file_end]))
                        .map_err(ElfLoadError::from)?;
                    load_segments += 1;

                    if file_start <= ph_offset && ph_end <= file_end {
                        phdr_address = start.checked_add(ph_offset - file_start);
                    }
                }
                xmas_elf::program::Type::Dynamic
                | xmas_elf::program::Type::Interp
                | xmas_elf::program::Type::Tls => return Err(ElfLoadError::InvalidElf),
                // PT_GNU_STACK(0x6474e551) 要求 X 时必须拒绝；忽略该 flag 会让程序在 NX 栈上以不同契约启动。
                xmas_elf::program::Type::OsSpecific(0x6474_e551) if ph.flags().is_execute() => {
                    return Err(ElfLoadError::InvalidElf);
                }
                _ => {}
            }
        }
        if load_segments == 0 {
            return Err(ElfLoadError::InvalidElf);
        }
        let phdr_address = phdr_address.ok_or(ElfLoadError::InvalidElf)?;

        let max_end_va: VirtualAddress = max_mapped_vpn.into();
        let heap_base = usize::from(max_end_va);
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

        let entry_point =
            usize::try_from(elf.header.pt2.entry_point()).map_err(|_| ElfLoadError::InvalidElf)?;
        let entry_pte = memory_set
            .translate(VirtualAddress::from(entry_point).floor())
            .ok_or(ElfLoadError::InvalidElf)?;
        if !entry_pte.flags().contains(PTEFlags::U | PTEFlags::X) {
            return Err(ElfLoadError::InvalidElf);
        }
        let phdr_pte = memory_set
            .translate(VirtualAddress::from(phdr_address).floor())
            .ok_or(ElfLoadError::InvalidElf)?;
        if !phdr_pte.flags().contains(PTEFlags::U | PTEFlags::R) {
            return Err(ElfLoadError::InvalidElf);
        }

        // 3. 初始栈是 argv/envp/auxv 的唯一用户契约，不通过寄存器传递私有参数。
        let aux = ElfAuxInfo {
            phdr: phdr_address,
            phent: ph_entry_size,
            phnum: ph_count,
            entry: entry_point,
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
        const AT_ENTRY: usize = 9;
        const AUX_WORDS: usize = 12;

        let total_string_size = args
            .iter()
            .chain(envs)
            .try_fold(0usize, |total, value| {
                value
                    .len()
                    .checked_add(1)
                    .and_then(|size| total.checked_add(size))
            })
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
            (AT_ENTRY, aux.entry),
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
}
