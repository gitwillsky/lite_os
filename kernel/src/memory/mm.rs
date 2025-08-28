use core::{arch::asm, error::Error, ops::Range};

use alloc::{boxed::Box, collections::BTreeMap, string::String, vec, vec::Vec};
use bitflags::bitflags;
use riscv::register::satp::{self, Satp};

use crate::memory::{
    address::{PhysicalAddress, PhysicalPageNumber, VirtualAddress},
    dynamic_linker::DynamicLinker,
    frame_allocator::{FrameTracker, alloc, alloc_contiguous},
    page_table::{PTEFlags, PageTableEntry, PageTableError},
    strampoline,
};

use super::config;
use super::{address::VirtualPageNumber, page_table::PageTable};

#[derive(Debug, Clone, Copy)]
pub enum MemoryError {
    OutOfMemory,
    PageTableError(PageTableError),
}

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

    pub fn copy_data(&mut self, page_table: &PageTable, data: &[u8]) {
        assert_eq!(self.map_type, MapType::Framed);
        let mut start: usize = 0;
        let mut current_vpn = self.vpn_range.start;
        let len = data.len();

        loop {
            let src = &data[start..len.min(start + config::PAGE_SIZE)];
            let pte = page_table
                .translate(current_vpn)
                .expect("Page table entry not found during data copy");
            let ppn = pte.ppn();
            let dst = &mut ppn.get_bytes_array_mut()[..src.len()];
            dst.copy_from_slice(src);
            start += config::PAGE_SIZE;
            if start >= len {
                break;
            }
            current_vpn = current_vpn.next();
        }
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
        let total = end.saturating_sub(start);

        // 第一阶段：仅解除页表映射（不触发 FrameTracker Drop）
        for vpn_usize in start..end {
            let vpn = VirtualPageNumber::from_vpn(vpn_usize);
            let _ = page_table.unmap(vpn);
        }

        // 第二阶段：集中回收物理帧，避免与容器节点释放的堆操作交织，降低锁递归风险
        if matches!(self.map_type, MapType::Framed) {
            let mut reclaimed_frames: Vec<FrameTracker> = Vec::new();
            for vpn_usize in start..end {
                let vpn = VirtualPageNumber::from_vpn(vpn_usize);
                if let Some(frame) = self.data_frames.remove(&vpn) {
                    reclaimed_frames.push(frame);
                }
            }
            // 在此处统一 Drop 帧，确保在不再持有 BTreeMap 节点的同时释放物理页，避免潜在死锁
            drop(reclaimed_frames);
        }
    }

    fn map_one(
        &mut self,
        page_table: &mut PageTable,
        vpn: VirtualPageNumber,
    ) -> Result<(), MemoryError> {
        let ppn: PhysicalPageNumber;
        match self.map_type {
            MapType::Framed => {
                let frame = alloc().ok_or(MemoryError::OutOfMemory)?;
                ppn = frame.ppn;
                self.data_frames.insert(vpn, frame);
            }
            MapType::Identical => {
                ppn = vpn.as_usize().into();
            }
        }

        let mut pte_flags = PTEFlags::from_bits(self.map_permission.bits()).unwrap();
        if self.global {
            pte_flags |= PTEFlags::G;
        }
        page_table.map(vpn, ppn, pte_flags)?;
        Ok(())
    }

    fn unmap_one(&mut self, page_table: &mut PageTable, vpn: VirtualPageNumber) {
        let _ = page_table.unmap(vpn);
        if let MapType::Framed = self.map_type {
            // 改为在 MapArea::unmap 中统一批量释放，避免与容器释放交错
            if let Some(frame) = self.data_frames.remove(&vpn) {
                core::mem::forget(frame);
            }
        }
    }

    pub fn shrink_to(&mut self, page_table: &mut PageTable, new_end: VirtualPageNumber) {
        for vpn in new_end.as_usize()..self.vpn_range.end.as_usize() {
            self.unmap_one(page_table, VirtualPageNumber::from_vpn(vpn));
        }
        self.vpn_range = Range {
            start: self.vpn_range.start,
            end: new_end,
        }
    }

    pub fn append_to(
        &mut self,
        page_table: &mut PageTable,
        new_end: VirtualPageNumber,
    ) -> Result<(), MemoryError> {
        for vpn in self.vpn_range.end.as_usize()..new_end.as_usize() {
            self.map_one(page_table, VirtualPageNumber::from_vpn(vpn))?;
        }
        self.vpn_range = Range {
            start: self.vpn_range.start,
            end: new_end,
        };
        Ok(())
    }

    pub fn from_another(another: &MapArea) -> Self {
        Self {
            vpn_range: another.vpn_range.clone(),
            data_frames: BTreeMap::new(),
            map_type: another.map_type,
            map_permission: another.map_permission,
            global: another.global,
        }
    }
}

#[derive(Debug)]
pub struct MemorySet {
    page_table: PageTable,
    areas: Vec<MapArea>,
    dynamic_linker: Option<DynamicLinker>,
    // 持有DMA连续页帧，避免在返回后被释放
    dma_allocations: Vec<FrameTracker>,
    // TLS template information
    tls_template: Option<TlsTemplate>,
    // Global pointer value for RISC-V
    global_pointer: Option<usize>,
}

/// TLS (Thread Local Storage) template information
#[derive(Debug, Clone)]
pub struct TlsTemplate {
    /// File size of the TLS initialization image
    pub file_size: usize,
    /// Memory size of the TLS segment (including .tbss)
    pub mem_size: usize,
    /// Alignment requirement
    pub align: usize,
    /// Virtual address of the TLS segment in the ELF
    pub vaddr: usize,
    /// TLS initialization data (tdata section)
    pub data: Vec<u8>,
}

impl MemorySet {
    pub fn new() -> Self {
        Self {
            page_table: PageTable::new(),
            areas: Vec::new(),
            dynamic_linker: None,
            dma_allocations: Vec::new(),
            tls_template: None,
            global_pointer: None,
        }
    }

    pub fn push(&mut self, mut map_area: MapArea, data: Option<&[u8]>) -> Result<(), MemoryError> {
        // 先尝试映射；若中途失败，需要回滚已映射的页面，避免留下半映射导致后续查找空闲区域极慢
        let start_vpn = map_area.vpn_range.start.as_usize();
        let end_vpn = map_area.vpn_range.end.as_usize();
        let pages = end_vpn.saturating_sub(start_vpn);
        if let Err(e) = map_area.map(&mut self.page_table) {
            // 回滚：解除已经映射的页面
            map_area.unmap(&mut self.page_table);
            return Err(e);
        }
        if let Some(data) = data {
            map_area.copy_data(&mut self.page_table, data);
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

    pub fn map_trampoline(&mut self) {
        let trampoline_va = VirtualAddress::from(config::TRAMPOLINE);
        let strampoline_pa = PhysicalAddress::from(strampoline as usize);

        // 忽略trampoline映射错误，某些情况下可能已经被映射了
        let _ = self.page_table.map(
            trampoline_va.into(),
            strampoline_pa.into(),
            // Trampoline 在所有地址空间通用，标记为 Global，避免跨进程切换时TLB混淆
            PTEFlags::R | PTEFlags::X | PTEFlags::G,
        );
    }

    pub fn active(&self) {
        let satp = self.page_table.token();
        unsafe {
            satp::write(Satp::from_bits(satp));
            asm!("sfence.vma")
        }
    }

    /// 跨核刷新：广播 IPI（SSIP）请求各核执行本地 sfence.vma
    pub fn flush_tlb_all_cpus() {
        // 本核先刷新
        unsafe { asm!("sfence.vma") }
        // 其他核通过 IPI 触发 SSIP，在软中断入口执行 sfence.vma
        crate::arch::sbi::sbi_send_ipi(usize::MAX, 0).ok();
    }

    pub fn get_page_table(&self) -> &PageTable {
        &self.page_table
    }

    pub fn get_page_table_mut(&mut self) -> &mut PageTable {
        &mut self.page_table
    }

    pub fn translate(&self, vpn: VirtualPageNumber) -> Option<PageTableEntry> {
        self.page_table.translate(vpn)
    }

    /// 在用户地址空间中查找空闲区域（按 VPN，从高到低）
    pub fn find_free_area_user(&self, length: usize) -> VirtualAddress {
        if length == 0 {
            return VirtualAddress::from(0);
        }

        let page_count = (length + config::PAGE_SIZE - 1) / config::PAGE_SIZE;
        // 用户空间仅使用低半区：bit38=0 的 canonical 范围
        // 直接使用低半区的最高 VPN 作为上界，避免误用高半区常量的低位（例如 TRAP_CONTEXT_BASE 的低 39 位）
        let upper_vpn_usize =
            ((1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1)) / config::PAGE_SIZE) - 1;
        if page_count == 0 || page_count > upper_vpn_usize {
            return VirtualAddress::from(0);
        }

        // 从最高可用 VPN 开始向下探测
        let mut start_vpn_usize = upper_vpn_usize.saturating_sub(page_count);
        while start_vpn_usize + page_count <= upper_vpn_usize {
            let mut is_free = true;
            for vpn_usize in start_vpn_usize..start_vpn_usize + page_count {
                if self
                    .translate(VirtualPageNumber::from_vpn(vpn_usize))
                    .is_some()
                {
                    is_free = false;
                    break;
                }
            }
            if is_free {
                return VirtualPageNumber::from_vpn(start_vpn_usize).into();
            }
            if start_vpn_usize == 0 {
                break;
            }
            start_vpn_usize -= 1;
        }
        VirtualAddress::from(0)
    }

    /// 分配DMA内存页面
    pub fn alloc_dma_pages(&mut self, page_count: usize) -> Result<PhysicalAddress, MemoryError> {
        if page_count == 0 {
            return Err(MemoryError::OutOfMemory);
        }

        // 分配连续物理页，适合DMA需求
        let frame = alloc_contiguous(page_count).ok_or(MemoryError::OutOfMemory)?;

        // 记录以保持生命周期，避免被Drop释放
        let phys_addr: PhysicalAddress = frame.ppn.into();
        self.dma_allocations.push(frame);

        Ok(phys_addr)
    }

    /// 映射DMA内存到虚拟地址空间
    pub fn map_dma(
        &mut self,
        phys_addr: PhysicalAddress,
        size: usize,
    ) -> Result<VirtualAddress, MemoryError> {
        // 选择位于低半区、远离内核物理恒等映射 (physmap) 的基址，避免与其重叠
        // 动态下界：max(4GiB, 物理内存末尾向上页对齐)
        let phys_end = crate::board::board_info().mem.end;
        let phys_end_aligned = (phys_end + config::PAGE_SIZE - 1) & !(config::PAGE_SIZE - 1);
        let mut base_candidate: usize = core::cmp::max(0x1_0000_0000usize, phys_end_aligned);
        let page_count = (size + config::PAGE_SIZE - 1) / config::PAGE_SIZE;

        // Sv39 低半区上界（不含），避免越界
        let low_half_limit: usize = 1usize << config::VIRTUAL_ADDRESS_WIDTH; // 2^39

        // 向上寻找一段连续未映射的虚拟区间
        'search: loop {
            // 越界保护：耗尽低半区虚拟地址空间
            if base_candidate + page_count * config::PAGE_SIZE > low_half_limit {
                return Err(MemoryError::OutOfMemory);
            }
            for i in 0..page_count {
                let va = VirtualAddress::from(base_candidate + i * config::PAGE_SIZE);
                if self.page_table.translate(va.floor()).is_some() {
                    // 该候选区间中存在已映射页，跳到下一候选窗口
                    base_candidate = base_candidate.saturating_add(page_count * config::PAGE_SIZE);
                    continue 'search;
                }
            }
            break; // 找到可用窗口
        }

        let dma_base = VirtualAddress::from(base_candidate);

        // 映射物理页面到虚拟地址
        for i in 0..page_count {
            let va = VirtualAddress::from(dma_base.as_usize() + i * config::PAGE_SIZE);
            let pa = PhysicalAddress::from(phys_addr.as_usize() + i * config::PAGE_SIZE);

            self.page_table
                .map(va.into(), pa.into(), PTEFlags::R | PTEFlags::W)?;
        }

        // 本核刷新 TLB，其他核通过 SSIP 在软中断入口刷新
        unsafe { asm!("sfence.vma") }

        Ok(dma_base)
    }

    /// 取消DMA内存映射
    pub fn unmap_dma(&mut self, virt_addr: VirtualAddress, size: usize) -> Result<(), MemoryError> {
        let page_count = (size + config::PAGE_SIZE - 1) / config::PAGE_SIZE;

        for i in 0..page_count {
            let va = VirtualAddress::from(virt_addr.as_usize() + i * config::PAGE_SIZE);
            self.page_table.unmap(va.into())?;
        }

        Ok(())
    }

    pub fn translate_va(&self, va: VirtualAddress) -> Option<PhysicalAddress> {
        self.page_table.translate_va(va)
    }
    
    /// Debug function to check if a virtual address is mapped
    pub fn is_mapped(&self, va: VirtualAddress) -> bool {
        self.page_table.translate(va.floor()).is_some()
    }

    pub fn shrink_to(&mut self, start: VirtualAddress, new_end: VirtualAddress) -> bool {
        if let Some(area) = self
            .areas
            .iter_mut()
            .find(|area| area.vpn_range.start == start.floor())
        {
            area.shrink_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
    }

    pub fn append_to(
        &mut self,
        start: VirtualAddress,
        new_end: VirtualAddress,
    ) -> Result<bool, MemoryError> {
        if let Some(area) = self
            .areas
            .iter_mut()
            .find(|area| area.vpn_range.start == start.floor())
        {
            area.append_to(&mut self.page_table, new_end.ceil())?;
            Ok(true)
        } else {
            Ok(false)
        }
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
        Self::from_elf_internal(elf_data, &[], &[], false)
    }

    /// Create a memory set from ELF data with dynamic linking support
    pub fn from_elf_with_dynamic_linking(
        elf_data: &[u8],
    ) -> Result<(Self, usize, usize), Box<dyn Error>> {
        Self::from_elf_internal(elf_data, &[], &[], true)
    }

    /// Internal ELF loading implementation with optional dynamic linking support
    fn from_elf_internal(
        elf_data: &[u8],
        args: &[String],
        envs: &[String],
        enable_dynamic_linking: bool,
    ) -> Result<(Self, usize, usize), Box<dyn Error>> {
        let mut memory_set = MemorySet::new();

        memory_set.map_trampoline();

        let elf = xmas_elf::ElfFile::new(elf_data)?;
        let elf_header = elf.header;
        let magic = elf_header.pt1.magic;
        assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf format");

        // Check if this is a dynamically linked executable
        let is_dynamic = elf_header.pt2.type_().as_type() == xmas_elf::header::Type::SharedObject
            || elf.find_section_by_name(".dynamic").is_some();
        
        // For PIE executables, we need a non-zero base address
        // Use 0x40000000 as a typical PIE base address
        let pie_base_address = if is_dynamic && elf_header.pt2.type_().as_type() == xmas_elf::header::Type::SharedObject {
            0x40000000usize
        } else {
            0usize
        };

        if enable_dynamic_linking && is_dynamic {
            info!("Loading dynamically linked ELF executable with base address 0x{:x}", pie_base_address);

            // For PIE executables, use the calculated base address
            let base_address = VirtualAddress::from(pie_base_address);
            
            // Initialize dynamic linker with base address
            let mut dynamic_linker = DynamicLinker::new_with_base(base_address);

            // Parse dynamic linking information
            dynamic_linker.parse_dynamic_elf(&elf, base_address)?;

            memory_set.dynamic_linker = Some(dynamic_linker);
        }

        let ph_count = elf_header.pt2.ph_count();
        let mut max_mapped_vpn = VirtualPageNumber::from(0);
        let mut plt_address = None;
        let mut got_address = None;
        let mut tls_info = None;
        let mut relro_range: Option<(VirtualAddress, VirtualAddress)> = None;

        // Process program headers
        for i in 0..ph_count {
            let ph = elf.program_header(i)?;

            match ph.get_type()? {
                xmas_elf::program::Type::Load => {
                    // Load regular segments
                    let start_va: VirtualAddress = (ph.virtual_addr() as usize + pie_base_address).into();
                    let end_va: VirtualAddress =
                        ((ph.virtual_addr() + ph.mem_size()) as usize + pie_base_address).into();

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
                    let map_area = MapArea::new(start_va, end_va, MapType::Framed, map_perm);

                    // 记录实际映射的最大页面号
                    max_mapped_vpn = max_mapped_vpn
                        .as_usize()
                        .max(map_area.vpn_range.end.as_usize())
                        .into();

                    debug!(
                        "Mapping LOAD segment: VA 0x{:x}-0x{:x}, flags: R={} W={} X={}",
                        start_va.as_usize(),
                        end_va.as_usize(),
                        ph_flags.is_read(),
                        ph_flags.is_write(),
                        ph_flags.is_execute()
                    );
                    
                    memory_set.push(
                        map_area,
                        Some(
                            &elf.input
                                [ph.offset() as usize..(ph.offset() + ph.file_size()) as usize],
                        ),
                    )?;
                }
                xmas_elf::program::Type::Dynamic => {
                    // Dynamic segment - already processed above
                    debug!("Found PT_DYNAMIC segment at 0x{:x}", ph.virtual_addr());
                }
                xmas_elf::program::Type::Interp => {
                    // Interpreter segment (dynamic linker path)
                    debug!("Found PT_INTERP segment");
                    if enable_dynamic_linking {
                        // In a real implementation, this would specify the dynamic linker to use
                        // For now, we use our built-in dynamic linker
                    }
                }
                xmas_elf::program::Type::Tls => {
                    // TLS segment
                    debug!("Found PT_TLS segment at 0x{:x}", ph.virtual_addr());
                    let tls_data = if ph.file_size() > 0 {
                        elf.input[ph.offset() as usize..(ph.offset() + ph.file_size()) as usize].to_vec()
                    } else {
                        Vec::new()
                    };
                    
                    tls_info = Some(TlsTemplate {
                        file_size: ph.file_size() as usize,
                        mem_size: ph.mem_size() as usize,
                        align: ph.align() as usize,
                        vaddr: ph.virtual_addr() as usize,
                        data: tls_data,
                    });
                }
                _ => {
                    // Other program header types - ignore for now
                }
            }
        }
        
        // Store TLS template in memory set
        memory_set.tls_template = tls_info;

        // If dynamic linking is enabled, find PLT and GOT sections
        if enable_dynamic_linking && memory_set.dynamic_linker.is_some() {
            if let Some(plt_section) = elf.find_section_by_name(".plt") {
                plt_address = Some(VirtualAddress::from(plt_section.address() as usize + pie_base_address));
                debug!("Found PLT section at 0x{:x}", plt_section.address() as usize + pie_base_address);
            }

            if let Some(got_section) = elf.find_section_by_name(".got") {
                got_address = Some(VirtualAddress::from(got_section.address() as usize + pie_base_address));
                debug!("Found GOT section at 0x{:x}", got_section.address() as usize + pie_base_address);
            }

            // Setup PLT if both PLT and GOT are present
            if let (Some(plt_addr), Some(got_addr)) = (plt_address, got_address) {
                if let Some(ref mut linker) = memory_set.dynamic_linker {
                    linker.setup_plt(plt_addr, got_addr)?;
                }
            }
        }

        // Place user stack at a fixed high address, below TRAP_CONTEXT
        // This ensures the stack is at a consistent location regardless of where code/data is loaded
        let user_stack_top = config::TRAP_CONTEXT_BASE - config::PAGE_SIZE; // Leave gap for guard page
        let user_stack_bottom = user_stack_top - config::USER_STACK_SIZE;

        memory_set.push(
            MapArea::new(
                user_stack_bottom.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        )?;

        // used in sbrk
        memory_set.push(
            MapArea::new(
                user_stack_top.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        )?;

        memory_set.push(
            MapArea::new(
                config::TRAP_CONTEXT_BASE.into(),
                config::TRAMPOLINE.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W,
            ),
            None,
        )?;

        let entry_point = (elf.header.pt2.entry_point() as usize) + pie_base_address;
        debug!("ELF entry point: raw=0x{:x}, with PIE base=0x{:x}", 
               elf.header.pt2.entry_point() as usize, entry_point);

        // Build argument stack if arguments are provided
        let actual_stack_top = if !args.is_empty() || !envs.is_empty() {
            memory_set.build_arg_stack(user_stack_top, args, envs, pie_base_address, entry_point, ph_count)?
        } else {
            user_stack_top
        };

        // Apply dynamic relocations if dynamic linking is enabled
        if enable_dynamic_linking && memory_set.dynamic_linker.is_some() {
            // Take the linker temporarily to avoid borrow conflicts
            if let Some(mut linker) = memory_set.dynamic_linker.take() {
                // Apply relocations
                {
                    let page_table = memory_set.get_page_table();
                    linker.apply_relocations(page_table)?;
                }
                // Run initializers
                linker.run_initializers()?;
                // Put linker back
                memory_set.dynamic_linker = Some(linker);
            }
        }
        
        // Parse and store global pointer for RISC-V
        memory_set.global_pointer = Self::parse_global_pointer(&elf, pie_base_address);
        if let Some(gp) = memory_set.global_pointer {
            debug!("Found __global_pointer$ symbol at 0x{:x}", gp);
        }
        
        // Apply RELRO protection after relocations (disabled for now)
        // TODO: Parse GNU_RELRO segment type properly from xmas_elf
        // if let Some((relro_start, relro_end)) = relro_range {
        //     info!("Applying RELRO protection to 0x{:x}-0x{:x}", relro_start.as_usize(), relro_end.as_usize());
        //     memory_set.apply_relro_protection(relro_start, relro_end)?;
        // }

        Ok((memory_set, actual_stack_top, entry_point))
    }

    /// Create a new memory set from ELF data with argument support
    pub fn from_elf_with_args(
        elf_data: &[u8],
        args: &[String],
        envs: &[String],
    ) -> Result<(Self, usize, usize), Box<dyn Error>> {
        Self::from_elf_internal(elf_data, args, envs, false)
    }

    /// Create a new memory set from ELF data with arguments and dynamic linking support
    pub fn from_elf_with_args_and_dynamic_linking(
        elf_data: &[u8],
        args: &[String],
        envs: &[String],
    ) -> Result<(Self, usize, usize), Box<dyn Error>> {
        Self::from_elf_internal(elf_data, args, envs, true)
    }

    /// Build argc/argv/envp layout on user stack
    fn build_arg_stack(
        &self,
        stack_top: usize,
        args: &[String],
        envs: &[String],
        base_address: usize,
        entry_point: usize,
        ph_count: u16,
    ) -> Result<usize, Box<dyn Error>> {
        let mut stack_ptr = stack_top;

        // Calculate total space needed for strings
        let mut total_string_size = 0;
        for arg in args {
            total_string_size += arg.len() + 1; // +1 for null terminator
        }
        for env in envs {
            total_string_size += env.len() + 1; // +1 for null terminator
        }

        // Align to 8 bytes boundary for arguments
        total_string_size = (total_string_size + 7) & !7;

        // Space for pointers: argc + argv[] + envp[] + aux vector + padding
        let argc = args.len();
        let aux_entries = 34; // 16 aux entries (PHDR, PHENT, PHNUM, PAGESZ, BASE, FLAGS, ENTRY, UID, EUID, GID, EGID, HWCAP, CLKTCK, SECURE, RANDOM, EXECFN) + AT_NULL, each with type+value = 17*2 = 34
        let pointer_space = core::mem::size_of::<usize>() * (1 + argc + 1 + envs.len() + 1 + aux_entries);
        // Add extra space for random data and padding
        let extra_space = 128; // More space to avoid overlap
        let pointer_space_aligned = ((pointer_space + extra_space) + 15) & !15;
        
        debug!("Memory layout: pointer_space={}, extra_space={}, aligned={}, total_string_size={}", 
               pointer_space, extra_space, pointer_space_aligned, total_string_size);

        // Move stack pointer down to accommodate everything
        stack_ptr -= total_string_size + pointer_space_aligned;
        stack_ptr &= !15; // Align to 16 bytes for RISC-V ABI

        let string_area_start = stack_ptr + pointer_space_aligned;
        let mut string_ptr = string_area_start;
        let mut argv_ptrs = Vec::new();
        let mut envp_ptrs = Vec::new();
        
        debug!("String area starts at 0x{:x}, writing strings", string_area_start);

        // Write argument strings and collect pointers
        for arg in args {
            debug!("Writing arg '{}' at 0x{:x}", arg, string_ptr);
            argv_ptrs.push(string_ptr);
            self.write_string_to_user_stack(string_ptr, arg)?;
            string_ptr += arg.len() + 1;
        }

        // Write environment strings and collect pointers
        for env in envs {
            envp_ptrs.push(string_ptr);
            self.write_string_to_user_stack(string_ptr, env)?;
            string_ptr += env.len() + 1;
        }

        // Write argc/argv/envp structure
        let mut ptr_writer = stack_ptr;

        // Write argc
        debug!("Writing argc={} at stack address 0x{:x}", argc, ptr_writer);
        self.write_usize_to_user_stack(ptr_writer, argc)?;
        ptr_writer += core::mem::size_of::<usize>();

        // Write argv pointers
        for &arg_ptr in &argv_ptrs {
            self.write_usize_to_user_stack(ptr_writer, arg_ptr)?;
            ptr_writer += core::mem::size_of::<usize>();
        }
        // Null terminator for argv
        self.write_usize_to_user_stack(ptr_writer, 0)?;
        ptr_writer += core::mem::size_of::<usize>();

        // Write envp pointers
        for &env_ptr in &envp_ptrs {
            self.write_usize_to_user_stack(ptr_writer, env_ptr)?;
            ptr_writer += core::mem::size_of::<usize>();
        }
        // Null terminator for envp
        self.write_usize_to_user_stack(ptr_writer, 0)?;
        ptr_writer += core::mem::size_of::<usize>();

        // musl expects auxiliary vector after envp
        // Follow Linux kernel order from fs/binfmt_elf.c
        
        // AT_PHDR (3) - program headers address
        self.write_usize_to_user_stack(ptr_writer, 3)?;
        ptr_writer += core::mem::size_of::<usize>();
        let phdr_addr = base_address + 64; // ELF64 header is 64 bytes
        debug!("AT_PHDR value: 0x{:x}", phdr_addr);
        self.write_usize_to_user_stack(ptr_writer, phdr_addr)?;
        ptr_writer += core::mem::size_of::<usize>();
        
        // AT_PHENT (4) - size of program header entry
        self.write_usize_to_user_stack(ptr_writer, 4)?;
        ptr_writer += core::mem::size_of::<usize>();
        self.write_usize_to_user_stack(ptr_writer, 56)?; // sizeof(Elf64_Phdr)
        ptr_writer += core::mem::size_of::<usize>();
        
        // AT_PHNUM (5) - number of program headers
        self.write_usize_to_user_stack(ptr_writer, 5)?;
        ptr_writer += core::mem::size_of::<usize>();
        self.write_usize_to_user_stack(ptr_writer, ph_count as usize)?;
        ptr_writer += core::mem::size_of::<usize>();
        
        // AT_PAGESZ (6) - system page size
        self.write_usize_to_user_stack(ptr_writer, 6)?;
        ptr_writer += core::mem::size_of::<usize>();
        self.write_usize_to_user_stack(ptr_writer, config::PAGE_SIZE)?;
        ptr_writer += core::mem::size_of::<usize>();
        
        // AT_BASE (7) - base address of interpreter (0 for static PIE without PT_INTERP)
        self.write_usize_to_user_stack(ptr_writer, 7)?;
        ptr_writer += core::mem::size_of::<usize>();
        self.write_usize_to_user_stack(ptr_writer, 0)?; // No interpreter loaded
        ptr_writer += core::mem::size_of::<usize>();
        
        // AT_FLAGS (8) - flags (0 for now)
        self.write_usize_to_user_stack(ptr_writer, 8)?;
        ptr_writer += core::mem::size_of::<usize>();
        self.write_usize_to_user_stack(ptr_writer, 0)?;
        ptr_writer += core::mem::size_of::<usize>();
        
        // AT_ENTRY (9) - program entry point
        self.write_usize_to_user_stack(ptr_writer, 9)?;
        ptr_writer += core::mem::size_of::<usize>();
        self.write_usize_to_user_stack(ptr_writer, entry_point)?;
        ptr_writer += core::mem::size_of::<usize>();
        
        // AT_UID (11) - real uid
        self.write_usize_to_user_stack(ptr_writer, 11)?;
        ptr_writer += core::mem::size_of::<usize>();
        self.write_usize_to_user_stack(ptr_writer, 0)?;
        ptr_writer += core::mem::size_of::<usize>();
        
        // AT_EUID (12) - effective uid
        self.write_usize_to_user_stack(ptr_writer, 12)?;
        ptr_writer += core::mem::size_of::<usize>();
        self.write_usize_to_user_stack(ptr_writer, 0)?;
        ptr_writer += core::mem::size_of::<usize>();
        
        // AT_GID (13) - real gid
        self.write_usize_to_user_stack(ptr_writer, 13)?;
        ptr_writer += core::mem::size_of::<usize>();
        self.write_usize_to_user_stack(ptr_writer, 0)?;
        ptr_writer += core::mem::size_of::<usize>();
        
        // AT_EGID (14) - effective gid
        self.write_usize_to_user_stack(ptr_writer, 14)?;
        ptr_writer += core::mem::size_of::<usize>();
        self.write_usize_to_user_stack(ptr_writer, 0)?;
        ptr_writer += core::mem::size_of::<usize>();
        
        // AT_HWCAP (16) - hardware capabilities
        self.write_usize_to_user_stack(ptr_writer, 16)?;
        ptr_writer += core::mem::size_of::<usize>();
        self.write_usize_to_user_stack(ptr_writer, 0)?;
        ptr_writer += core::mem::size_of::<usize>();
        
        // AT_CLKTCK (17) - clock ticks per second
        self.write_usize_to_user_stack(ptr_writer, 17)?;
        ptr_writer += core::mem::size_of::<usize>();
        self.write_usize_to_user_stack(ptr_writer, 100)?; // 100 Hz
        ptr_writer += core::mem::size_of::<usize>();
        
        // AT_SECURE (23) - secure mode flag
        self.write_usize_to_user_stack(ptr_writer, 23)?;
        ptr_writer += core::mem::size_of::<usize>();
        self.write_usize_to_user_stack(ptr_writer, 0)?;
        ptr_writer += core::mem::size_of::<usize>();
        
        // AT_RANDOM (25) - pointer to 16 random bytes
        self.write_usize_to_user_stack(ptr_writer, 25)?;
        ptr_writer += core::mem::size_of::<usize>();
        // Place random data safely away from string area
        let random_data_addr = stack_ptr + pointer_space_aligned - 64; // Well before string area
        debug!("Setting AT_RANDOM to 0x{:x} (string area starts at 0x{:x})", 
               random_data_addr, stack_ptr + pointer_space_aligned);
        self.write_usize_to_user_stack(ptr_writer, random_data_addr)?;
        ptr_writer += core::mem::size_of::<usize>();
        
        // AT_EXECFN (31) - file name of program  
        self.write_usize_to_user_stack(ptr_writer, 31)?;
        ptr_writer += core::mem::size_of::<usize>();
        // Point to the first argument string (program name)
        debug!("AT_EXECFN pointing to argv[0] at 0x{:x}", argv_ptrs[0]);
        self.write_usize_to_user_stack(ptr_writer, argv_ptrs[0])?;
        ptr_writer += core::mem::size_of::<usize>();
        
        // AT_NULL (0) - end of aux vector
        self.write_usize_to_user_stack(ptr_writer, 0)?;
        ptr_writer += core::mem::size_of::<usize>();
        self.write_usize_to_user_stack(ptr_writer, 0)?;
        ptr_writer += core::mem::size_of::<usize>();

        // Write 16 bytes of fake random data for AT_RANDOM
        debug!("Writing fake random data at 0x{:x}", random_data_addr);
        for i in 0..2 {
            // Write some non-zero values as fake random data (avoid values that look like addresses)
            self.write_usize_to_user_stack(random_data_addr + i * 8, 0x0102030405060708 + i as usize)?;
        }
        
        // Write zero padding after random data to avoid access violations
        for i in 2..16 {  // More padding to cover the fault address
            self.write_usize_to_user_stack(random_data_addr + i * 8, 0)?;
        }
        
        debug!("Random data covers 0x{:x} to 0x{:x}, fault at 0x{:x}", 
               random_data_addr, random_data_addr + 16 * 8, 0xfffffffffffbdfd0_usize);
        
        debug!("build_arg_stack returning stack_ptr=0x{:x} (original stack_top was 0x{:x})", 
               stack_ptr, stack_top);
        debug!("Stack layout: argc at 0x{:x}, argv at 0x{:x}, envp at 0x{:x}", 
               stack_ptr, stack_ptr + 8, stack_ptr + 8 + (argc + 1) * 8);
        let auxv_start = stack_ptr + 8 + (argc + 1) * 8 + (envs.len() + 1) * 8;
        debug!("Auxiliary vector starts at 0x{:x}", auxv_start);
        debug!("Fault address 0xfffffffffffbdfd0 is at offset 0x{:x} from stack_ptr", 
               0xfffffffffffbdfd0_usize.wrapping_sub(stack_ptr));
        debug!("First aux entries: PHDR @ 0x{:x}, PHENT @ 0x{:x}, PHNUM @ 0x{:x}", 
               auxv_start, auxv_start + 16, auxv_start + 32);
        debug!("Key aux entries: PAGESZ @ 0x{:x}, BASE @ 0x{:x}, ENTRY @ 0x{:x}", 
               auxv_start + 48, auxv_start + 64, auxv_start + 96);
        let at_null_offset = 16 * 16; // 16 entries before AT_NULL, each 16 bytes
        debug!("AT_NULL @ 0x{:x}, fault offset from AT_NULL: 0x{:x}", 
               auxv_start + at_null_offset, 
               0xfffffffffffbdfd0_usize.wrapping_sub(auxv_start + at_null_offset));
        Ok(stack_ptr)
    }

    /// Write a string to user stack memory
    fn write_string_to_user_stack(&self, addr: usize, s: &str) -> Result<(), Box<dyn Error>> {
        let vpn_start = VirtualAddress::from(addr).floor();
        let vpn_end = VirtualAddress::from(addr + s.len() + 1).floor();

        for vpn in vpn_start.as_usize()..=vpn_end.as_usize() {
            let vpn = VirtualPageNumber::from_vpn(vpn);
            if let Some(pte) = self.translate(vpn) {
                let ppn = pte.ppn();
                let page_bytes = ppn.get_bytes_array_mut();

                let page_start = vpn.as_usize() * config::PAGE_SIZE;
                let page_end = page_start + config::PAGE_SIZE;

                let str_start = addr.max(page_start);
                let str_end = (addr + s.len()).min(page_end);

                if str_start < str_end {
                    let page_offset = str_start - page_start;
                    let str_offset = str_start - addr;
                    let copy_len = str_end - str_start;

                    page_bytes[page_offset..page_offset + copy_len]
                        .copy_from_slice(&s.as_bytes()[str_offset..str_offset + copy_len]);
                }

                // Write null terminator if this page contains the end
                if addr + s.len() >= page_start && addr + s.len() < page_end {
                    let null_offset = (addr + s.len()) - page_start;
                    page_bytes[null_offset] = 0;
                }
            }
        }
        Ok(())
    }

    /// Write a usize value to user stack memory
    fn write_usize_to_user_stack(&self, addr: usize, value: usize) -> Result<(), Box<dyn Error>> {
        let bytes = value.to_le_bytes();
        let vpn_start = VirtualAddress::from(addr).floor();
        let vpn_end = VirtualAddress::from(addr + core::mem::size_of::<usize>() - 1).floor();

        for vpn in vpn_start.as_usize()..=vpn_end.as_usize() {
            let vpn = VirtualPageNumber::from_vpn(vpn);
            if let Some(pte) = self.translate(vpn) {
                let ppn = pte.ppn();
                let page_bytes = ppn.get_bytes_array_mut();

                let page_start = vpn.as_usize() * config::PAGE_SIZE;
                let page_end = page_start + config::PAGE_SIZE;

                let val_start = addr.max(page_start);
                let val_end = (addr + core::mem::size_of::<usize>()).min(page_end);

                if val_start < val_end {
                    let page_offset = val_start - page_start;
                    let val_offset = val_start - addr;
                    let copy_len = val_end - val_start;

                    page_bytes[page_offset..page_offset + copy_len]
                        .copy_from_slice(&bytes[val_offset..val_offset + copy_len]);
                }
            }
        }
        Ok(())
    }

    pub fn form_existed_user(user_space: &MemorySet) -> Result<Self, MemoryError> {
        let mut memory_set = MemorySet::new();
        memory_set.map_trampoline();

        // Clone TLS template
        memory_set.tls_template = user_space.tls_template.clone();

        for area in user_space.areas.iter() {
            let new_area = MapArea::from_another(area);
            memory_set.push(new_area, None)?;

            for vpn in area.vpn_range.start.as_usize()..area.vpn_range.end.as_usize() {
                let vpn = VirtualPageNumber::from_vpn(vpn);
                let src_ppn = user_space
                    .translate(vpn)
                    .expect("Source page table entry not found during clone")
                    .ppn();
                let dst_ppn = memory_set
                    .translate(vpn)
                    .expect("Destination page table entry not found during clone")
                    .ppn();
                dst_ppn
                    .get_bytes_array_mut()
                    .copy_from_slice(&src_ppn.get_bytes_array_mut());
            }
        }
        Ok(memory_set)
    }

    pub fn recycle_data_pages(&mut self) {
        self.areas.clear();
    }

    /// Get a reference to the dynamic linker
    pub fn get_dynamic_linker(&self) -> Option<&DynamicLinker> {
        self.dynamic_linker.as_ref()
    }

    /// Get a mutable reference to the dynamic linker
    pub fn get_dynamic_linker_mut(&mut self) -> Option<&mut DynamicLinker> {
        self.dynamic_linker.as_mut()
    }

    /// Load a shared library at runtime
    pub fn load_shared_library(
        &mut self,
        library_name: &str,
    ) -> Result<VirtualAddress, Box<dyn Error>> {
        if self.dynamic_linker.is_none() {
            return Err("Dynamic linker not initialized".into());
        }

        // Take the linker temporarily to avoid double borrow
        if let Some(mut linker) = self.dynamic_linker.take() {
            let result = linker.load_shared_library(self, library_name);
            self.dynamic_linker = Some(linker);
            result
        } else {
            Err("Dynamic linker not available".into())
        }
    }

    /// Resolve a symbol by name across all loaded libraries
    pub fn resolve_symbol(&self, symbol_name: &str) -> Option<VirtualAddress> {
        if let Some(ref linker) = self.dynamic_linker {
            linker.resolve_symbol(symbol_name)
        } else {
            None
        }
    }

    /// Get the global pointer value for this memory set
    pub fn get_global_pointer(&self) -> Option<usize> {
        self.global_pointer
    }

    /// Allocate and initialize TLS (Thread Local Storage) for a thread
    /// Returns the address that should be loaded into the tp register
    pub fn allocate_tls(&mut self) -> Result<usize, MemoryError> {
        // Clone template data to avoid borrow issues
        let template_info = if let Some(ref template) = self.tls_template {
            Some((
                template.file_size,
                template.mem_size,
                template.align,
                template.data.clone(),
            ))
        } else {
            None
        };
        
        if let Some((file_size, mem_size, align, data)) = template_info {
            // Calculate total TLS size including TCB (Thread Control Block)
            // Layout: [TLS data] [padding] [TCB]
            // tp points to the end of TLS data (start of TCB)
            
            // Align TLS size to template alignment
            let tls_size = (mem_size + align - 1) & !(align - 1);
            
            // TCB size for musl compatibility (minimum 2 pointers)
            const TCB_SIZE: usize = 16; // 2 * size_of::<usize>()
            
            // Total allocation size
            let total_size = tls_size + TCB_SIZE;
            
            // Find a free area in user space
            let tls_base_va = self.find_free_area_user(total_size);
            if tls_base_va.as_usize() == 0 {
                return Err(MemoryError::OutOfMemory);
            }
            
            // Map TLS area
            let tls_area = MapArea::new(
                tls_base_va,
                VirtualAddress::from(tls_base_va.as_usize() + total_size),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            );
            
            // Map the area
            self.push(tls_area, None)?;
            
            // Initialize TLS data
            if file_size > 0 {
                // Copy tdata section
                self.write_user_data(tls_base_va, &data[..file_size])?;
            }
            
            // Zero-fill tbss section (mem_size - file_size)
            if mem_size > file_size {
                let zero_size = mem_size - file_size;
                let zeros = vec![0u8; zero_size];
                self.write_user_data(
                    VirtualAddress::from(tls_base_va.as_usize() + file_size),
                    &zeros,
                )?;
            }
            
            // tp points to the end of TLS data (aligned)
            let tp_value = tls_base_va.as_usize() + tls_size;
            
            // Initialize TCB
            // TCB[0] = tp (self-pointer for musl compatibility)
            debug!("Setting TLS self-pointer: writing 0x{:x} to address 0x{:x}", tp_value, tp_value);
            self.write_user_usize(tp_value, tp_value)?;
            
            // Verify the write was successful
            match self.read_user_usize(tp_value) {
                Ok(value) => {
                    if value == tp_value {
                        debug!("TLS self-pointer verification successful");
                    } else {
                        error!("TLS self-pointer verification failed: read 0x{:x}, expected 0x{:x}", value, tp_value);
                    }
                }
                Err(e) => {
                    error!("TLS self-pointer read verification failed: {:?}", e);
                }
            }
            
            Ok(tp_value)
        } else {
            // No TLS needed, tp can be 0
            Ok(0)
        }
    }

    /// Write data to user memory
    fn write_user_data(&self, addr: VirtualAddress, data: &[u8]) -> Result<(), MemoryError> {
        let mut current_addr = addr.as_usize();
        let mut offset = 0;
        
        while offset < data.len() {
            let vpn = VirtualAddress::from(current_addr).floor();
            if let Some(pte) = self.translate(vpn) {
                let ppn = pte.ppn();
                let page_offset = current_addr & (config::PAGE_SIZE - 1);
                let bytes_to_copy = core::cmp::min(config::PAGE_SIZE - page_offset, data.len() - offset);
                
                let dst = &mut ppn.get_bytes_array_mut()[page_offset..page_offset + bytes_to_copy];
                dst.copy_from_slice(&data[offset..offset + bytes_to_copy]);
                
                offset += bytes_to_copy;
                current_addr += bytes_to_copy;
            } else {
                return Err(MemoryError::OutOfMemory);
            }
        }
        
        Ok(())
    }

    /// Write a usize to user memory
    fn write_user_usize(&self, addr: usize, value: usize) -> Result<(), MemoryError> {
        let bytes = value.to_le_bytes();
        self.write_user_data(VirtualAddress::from(addr), &bytes)
    }

    /// Read a usize value from user memory
    fn read_user_usize(&self, addr: usize) -> Result<usize, MemoryError> {
        let addr_va = VirtualAddress::from(addr);
        let mut buffer = [0u8; 8]; // usize is 8 bytes on 64-bit
        
        let mut current_addr = addr_va.as_usize();
        let mut offset = 0;
        
        while offset < buffer.len() {
            if let Some(pa) = self.translate_va(VirtualAddress::from(current_addr)) {
                let page_offset = current_addr & (config::PAGE_SIZE - 1);
                let bytes_to_read = core::cmp::min(config::PAGE_SIZE - page_offset, buffer.len() - offset);
                
                unsafe {
                    let src_ptr = pa.as_usize() as *const u8;
                    core::ptr::copy_nonoverlapping(src_ptr, buffer.as_mut_ptr().add(offset), bytes_to_read);
                }
                
                current_addr += bytes_to_read;
                offset += bytes_to_read;
            } else {
                return Err(MemoryError::OutOfMemory);
            }
        }
        
        Ok(usize::from_le_bytes(buffer))
    }

    /// Get TLS template
    pub fn get_tls_template(&self) -> Option<&TlsTemplate> {
        self.tls_template.as_ref()
    }
    
    /// Apply RELRO (Read-Only after Relocation) protection
    /// This makes the GOT and other relocation data read-only after relocations are applied
    pub fn apply_relro_protection(&mut self, start: VirtualAddress, end: VirtualAddress) -> Result<(), MemoryError> {
        debug!("Applying RELRO protection to range 0x{:x}-0x{:x}", start.as_usize(), end.as_usize());
        
        let start_vpn = start.floor();
        let end_vpn = end.ceil();
        
        // Change permissions to read-only
        for vpn in start_vpn.as_usize()..end_vpn.as_usize() {
            let vpn = VirtualPageNumber::from_vpn(vpn);
            if let Some(pte) = self.page_table.translate(vpn) {
                let ppn = pte.ppn();
                // Remove write permission, keep read and user permissions
                let new_flags = (pte.flags() & !PTEFlags::W) | PTEFlags::R | PTEFlags::U;
                self.page_table.unmap(vpn)?;
                self.page_table.map(vpn, ppn, new_flags)?;
            }
        }
        
        // Flush TLB for the modified pages
        unsafe { core::arch::asm!("sfence.vma") }
        
        Ok(())
    }

    /// Parse the __global_pointer$ symbol from ELF file
    fn parse_global_pointer(elf: &xmas_elf::ElfFile, base_address: usize) -> Option<usize> {
        use xmas_elf::sections::{SectionData, SectionHeader};
        use xmas_elf::symbol_table::{Entry, Type};

        // Find symbol table sections
        for section in elf.section_iter() {
            match section.get_data(elf) {
                Ok(SectionData::SymbolTable32(symbol_table)) => {
                    for symbol in symbol_table {
                        if let Ok(name) = symbol.get_name(elf) {
                            if name == "__global_pointer$" {
                                let gp_addr = symbol.value() as usize + base_address;
                                debug!("Found __global_pointer$ at 0x{:x} (base: 0x{:x}, offset: 0x{:x})", 
                                       gp_addr, base_address, symbol.value());
                                return Some(gp_addr);
                            }
                        }
                    }
                }
                Ok(SectionData::SymbolTable64(symbol_table)) => {
                    for symbol in symbol_table {
                        if let Ok(name) = symbol.get_name(elf) {
                            if name == "__global_pointer$" {
                                let gp_addr = symbol.value() as usize + base_address;
                                debug!("Found __global_pointer$ at 0x{:x} (base: 0x{:x}, offset: 0x{:x})", 
                                       gp_addr, base_address, symbol.value());
                                return Some(gp_addr);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // If not found in symbol table, calculate default gp for RISC-V
        // Global pointer typically points to the middle of the small data sections
        if let Some(sdata_section) = elf.find_section_by_name(".sdata") {
            let gp_offset = sdata_section.address() as usize + 0x800; // Standard RISC-V offset
            let gp_addr = gp_offset + base_address;
            debug!("Using calculated __global_pointer$ at 0x{:x} based on .sdata section", gp_addr);
            return Some(gp_addr);
        }

        // For PIE executables without explicit gp symbol, let the program set its own gp
        // But we need to provide a safe initial value to avoid crashes before gp is set
        // Use 0x800 as a traditional value that will be adjusted by the program's startup code
        if base_address != 0 {
            let default_gp = base_address + 0x800;
            debug!("Using default __global_pointer$ at 0x{:x} for PIE executable", default_gp);
            return Some(default_gp);
        }

        debug!("__global_pointer$ symbol not found and no .sdata section available");
        None
    }
}
