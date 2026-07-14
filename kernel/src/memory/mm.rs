mod cow;
mod executable_load;
mod futex_key;
mod initial_stack;
mod mapping_request;
mod mmap;
mod private_area;
mod process;
mod shared_area;
mod user_access;
use super::config;
use super::executable::{ElfKind, ExecutableImage};
use super::{address::VirtualPageNumber, page_table::PageTable};
use crate::memory::{
    SharedFileError, SharedFileId, SharedFileMapping, SharedPage,
    address::{PhysicalAddress, PhysicalPageNumber, VirtualAddress},
    frame_allocator::{FrameTracker, alloc},
    page_table::{PTEFlags, PageTableEntry, PageTableError},
    strampoline,
};
use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec::Vec,
};
use bitflags::bitflags;
use core::{arch::asm, error::Error, ops::Range};
use initial_stack::ElfAuxInfo;
use private_area::PrivateFileArea;
use riscv::register::satp::{self, Satp};
use shared_area::{AnonymousSharedBacking, SharedAnonymousArea, SharedFileArea, SharedResident};
pub(crate) use {
    futex_key::FutexKey,
    mapping_request::{FileMappingSource, MappingResourceLimits, MemoryAdvice},
    mmap::{PageFaultAccess, PageFaultOutcome},
    user_access::UserFaultLimits,
};
#[derive(Debug, Clone, Copy)]
pub(crate) enum MemoryError {
    OutOfMemory,
    PageTableError(PageTableError),
    InvalidRange,
    AddressInUse,
    PermissionDenied,
    Io,
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
            MemoryError::Io => write!(f, "File-backed memory I/O failed"),
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
    /// executable source 在 transaction 构造期间发生 I/O error 或 short read。
    Io,
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
            MemoryError::Io => Self::Io,
        }
    }
}

impl core::fmt::Display for ElfLoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutOfMemory => write!(f, "out of memory while loading ELF"),
            Self::InvalidElf => write!(f, "invalid or unsupported RV64 ELF image"),
            Self::Io => write!(f, "I/O error while loading ELF"),
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
    Stack {
        top: usize,
    },
    Elf,
    File,
}

#[derive(Debug)]
pub(crate) struct MapArea {
    vpn_range: Range<VirtualPageNumber>,
    data_page_offset: usize,
    data_frames: BTreeMap<VirtualPageNumber, Arc<FrameTracker>>,
    map_type: MapType,
    map_permission: MapPermission,
    /// 是否标记为全局页（G位）。仅用于内核空间映射。
    global: bool,
    kind: VmaKind,
    shared_anonymous: Option<SharedAnonymousArea>,
    shared_file: Option<SharedFileArea>,
    private_file: Option<PrivateFileArea>,
    /// private VMA 只声明地址范围，首次访问才分配物理页。
    lazy_private: bool,
    /// MADV_FREE 标记的 private anonymous resident；write fault 会取消标记。
    discardable: BTreeSet<VirtualPageNumber>,
    /// file-backed MAP_PRIVATE 已发生写入的 resident；干净页可从 backing 重建并回收。
    dirty_private: BTreeSet<VirtualPageNumber>,
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
            shared_anonymous: None,
            shared_file: None,
            private_file: None,
            lazy_private: false,
            discardable: BTreeSet::new(),
            dirty_private: BTreeSet::new(),
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
        for (index, frame) in self.data_frames.values_mut().enumerate() {
            let page_offset = if index == 0 { self.data_page_offset } else { 0 };
            let count = (config::PAGE_SIZE - page_offset).min(data.len() - copied);
            Arc::get_mut(frame)
                .expect("new mapping frame must be uniquely owned")
                .bytes_mut()[page_offset..page_offset + count]
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
        if self.lazy_private || self.shared_anonymous.is_some() || self.shared_file.is_some() {
            for vpn in self.data_frames.keys().copied() {
                let _ = page_table.unmap(vpn);
            }
            if let Some(shared) = &self.shared_file {
                for vpn in shared.resident.keys().copied() {
                    let _ = page_table.unmap(vpn);
                }
            }
        } else {
            for vpn in self.vpn_range.start.as_usize()..self.vpn_range.end.as_usize() {
                let _ = page_table.unmap(VirtualPageNumber::from_vpn(vpn));
            }
        }
        self.data_frames.clear();
        self.discardable.clear();
        self.dirty_private.clear();
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
            let replaced = self.data_frames.insert(vpn, Arc::new(frame));
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
        if self.lazy_private {
            self.vpn_range.end = new_end;
            return Ok(());
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
        let right_discardable = self.discardable.split_off(&end);
        let middle_discardable = self.discardable.split_off(&start);
        let right_dirty = self.dirty_private.split_off(&end);
        let middle_dirty = self.dirty_private.split_off(&start);
        let (left_shared, middle_shared, right_shared) = if let Some(mut shared) = self.shared_file
        {
            let right_pages = shared.resident.split_off(&end);
            let middle_pages = shared.resident.split_off(&start);
            let base = shared.file_offset;
            let page_bytes = config::PAGE_SIZE as u64;
            let middle_offset =
                base + (start.as_usize() - original_start.as_usize()) as u64 * page_bytes;
            let right_offset =
                base + (end.as_usize() - original_start.as_usize()) as u64 * page_bytes;
            let mapping = shared.mapping;
            (
                Some(SharedFileArea {
                    mapping: mapping.clone(),
                    file_offset: base,
                    resident: shared.resident,
                }),
                Some(SharedFileArea {
                    mapping: mapping.clone(),
                    file_offset: middle_offset,
                    resident: middle_pages,
                }),
                Some(SharedFileArea {
                    mapping,
                    file_offset: right_offset,
                    resident: right_pages,
                }),
            )
        } else {
            (None, None, None)
        };
        let (left_anonymous, middle_anonymous, right_anonymous) =
            SharedAnonymousArea::partition(self.shared_anonymous, original_start, start, end);
        let kind = self.kind;
        let build = |range: Range<VirtualPageNumber>,
                     data_frames,
                     discardable,
                     dirty_private,
                     shared_anonymous,
                     shared_file| Self {
            vpn_range: range,
            data_page_offset: 0,
            data_frames,
            map_type: MapType::Framed,
            map_permission: self.map_permission,
            global: false,
            kind,
            shared_anonymous,
            shared_file,
            private_file: self.private_file.clone(),
            lazy_private: self.lazy_private,
            discardable,
            dirty_private,
        };
        let left = (original_start < start).then(|| {
            build(
                original_start..start,
                self.data_frames,
                self.discardable,
                self.dirty_private,
                left_anonymous,
                left_shared,
            )
        });
        let middle = build(
            start..end,
            middle_frames,
            middle_discardable,
            middle_dirty,
            middle_anonymous,
            middle_shared,
        );
        let right = (end < original_end).then(|| {
            build(
                end..original_end,
                right_frames,
                right_discardable,
                right_dirty,
                right_anonymous,
                right_shared,
            )
        });
        (left, middle, right)
    }

    fn merge_anonymous(mut self, mut right: Self) -> Self {
        debug_assert_eq!(self.kind, VmaKind::Anonymous);
        debug_assert_eq!(right.kind, VmaKind::Anonymous);
        debug_assert_eq!(self.vpn_range.end, right.vpn_range.start);
        debug_assert_eq!(self.map_permission, right.map_permission);
        self.vpn_range.end = right.vpn_range.end;
        self.data_frames.append(&mut right.data_frames);
        self.discardable.append(&mut right.discardable);
        self.dirty_private.append(&mut right.dirty_private);
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
            shared_anonymous: None,
            shared_file: None,
            private_file: self.private_file.clone(),
            lazy_private: self.lazy_private,
            discardable: self.discardable.clone(),
            dirty_private: self.dirty_private.clone(),
        };
        if let Err(error) = cloned.map(page_table) {
            cloned.unmap(page_table);
            return Err(error);
        }
        for (vpn, source) in &self.data_frames {
            Arc::get_mut(
                cloned
                    .data_frames
                    .get_mut(vpn)
                    .expect("cloned framed VMA must own every source VPN"),
            )
            .expect("eager-cloned system frame must be unique")
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
    // OWNER: Linux mm 的 arg_start/arg_end；缺失时 procfs 只能伪造静态 argv，无法反映用户栈修改。
    argument_range: Range<usize>,
}

impl MemorySet {
    const MMAP_BASE: usize = 0x4000_0000;

    pub(crate) fn new() -> Self {
        Self {
            page_table: PageTable::new(),
            areas: BTreeMap::new(),
            argument_range: 0..0,
        }
    }

    fn try_new() -> Result<Self, MemoryError> {
        Ok(Self {
            page_table: PageTable::try_new()?,
            areas: BTreeMap::new(),
            argument_range: 0..0,
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

    /// @description 返回用户 VMA `(virtual_pages, resident_pages)`；不计 kernel-only trap context。
    pub(crate) fn user_page_statistics(&self) -> (usize, usize) {
        self.areas
            .values()
            .filter(|area| area.map_permission.contains(MapPermission::U))
            .fold((0, 0), |(virtual_pages, resident_pages), area| {
                (
                    virtual_pages + area.vpn_range.end.as_usize() - area.vpn_range.start.as_usize(),
                    resident_pages
                        + area.data_frames.len()
                        + area
                            .shared_file
                            .as_ref()
                            .map_or(0, |shared| shared.resident.len()),
                )
            })
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
                    && matches!(
                        area.kind,
                        VmaKind::Anonymous | VmaKind::Heap { .. } | VmaKind::Elf | VmaKind::File
                    )
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
        if base >= limit || !VirtualAddress::from(base).is_aligned() {
            return Err(MemoryError::InvalidRange);
        }
        self.push(MapArea::heap(base, limit), None)
    }

    /// @description 查询或原子提交 program break；失败保持旧值。
    pub(crate) fn set_program_break(
        &mut self,
        new_break: usize,
        address_space_limit: u64,
        data_limit: u64,
    ) -> Result<usize, MemoryError> {
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
        if new_page_end > old_page_end {
            let additional = (new_page_end.as_usize() - old_page_end.as_usize()) as u64
                * config::PAGE_SIZE as u64;
            self.ensure_resource_capacity(additional, address_space_limit, Some(data_limit))?;
        }
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

    fn grow_stack_for_fault(
        &mut self,
        address: usize,
        stack_limit: u64,
        address_space_limit: u64,
    ) -> Result<(), MemoryError> {
        let target = VirtualAddress::from(address).floor();
        let Some((key, top)) = self.areas.iter().find_map(|(key, area)| match area.kind {
            VmaKind::Stack { top } => Some((*key, top)),
            _ => None,
        }) else {
            return Ok(());
        };
        if target >= key {
            return Ok(());
        }
        let target_address = target.as_usize() * config::PAGE_SIZE;
        let stack_limit = usize::try_from(stack_limit).unwrap_or(usize::MAX);
        let allowed_bottom = top.saturating_sub(stack_limit);
        if target_address < allowed_bottom {
            return Ok(());
        }
        if self
            .areas
            .range(..key)
            .next_back()
            .is_some_and(|(_, previous)| {
                previous.vpn_range.end.as_usize().saturating_add(1) > target.as_usize()
            })
        {
            return Ok(());
        }
        let additional = (key.as_usize() - target.as_usize()) as u64 * config::PAGE_SIZE as u64;
        self.ensure_resource_capacity(additional, address_space_limit, None)?;
        let mut area = self.areas.remove(&key).expect("stack key must remain live");
        area.vpn_range.start = target;
        self.areas.insert(target, area);
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

    /// @description 从已校验 ELF plan 构造受 rlimit 约束的新地址空间、初始栈与 entry。
    pub(crate) fn from_elf(
        image: &ExecutableImage,
        args: &[Vec<u8>],
        envs: &[Vec<u8>],
        execfn: &[u8],
        stack_limit: u64,
        address_space_limit: u64,
        data_limit: u64,
    ) -> Result<(Self, usize, usize), ElfLoadError> {
        let mut memory_set = MemorySet::try_new().map_err(ElfLoadError::from)?;
        memory_set.map_trampoline().map_err(ElfLoadError::from)?;
        const MAIN_PIE_BASE: usize = 0x1_0000;
        const INTERPRETER_BASE: usize = 0x2000_0000;
        let main_type = image.main.kind;
        let main_bias = match main_type {
            ElfKind::Executable if image.interpreter.is_none() => 0,
            ElfKind::SharedObject if image.interpreter.is_some() => MAIN_PIE_BASE,
            _ => return Err(ElfLoadError::InvalidElf),
        };
        let main = memory_set.map_elf_image(&image.main, main_bias)?;
        let (entry_point, interpreter_base) = if let Some(interpreter) = &image.interpreter {
            if interpreter.kind != ElfKind::SharedObject {
                return Err(ElfLoadError::InvalidElf);
            }
            let loaded = memory_set.map_elf_image(interpreter, INTERPRETER_BASE)?;
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
        let heap_limit = user_stack_top
            .checked_sub(config::PAGE_SIZE)
            .ok_or(ElfLoadError::InvalidElf)?;
        if heap_base >= heap_limit {
            return Err(ElfLoadError::InvalidElf);
        }

        // 2. heap 从最高 LOAD 末端开始；栈位于 Sv39 低半区顶部，上下各保留一页 guard。
        memory_set
            .push(MapArea::stack(user_stack_top), None)
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

        if memory_set.handle_page_fault(phdr_address, PageFaultAccess::Read)?
            != PageFaultOutcome::Handled
        {
            return Err(ElfLoadError::InvalidElf);
        }
        let phdr_pte = memory_set
            .translate(VirtualAddress::from(phdr_address).floor())
            .ok_or(ElfLoadError::InvalidElf)?;
        if !phdr_pte.flags().contains(PTEFlags::U | PTEFlags::R) {
            return Err(ElfLoadError::InvalidElf);
        }

        // 3. 初始栈是 argv/envp/auxv 的唯一用户契约，不通过寄存器传递私有参数。
        let aux = ElfAuxInfo::new(
            phdr_address,
            main.phent,
            main.phnum,
            main.entry,
            interpreter_base,
        );
        let actual_stack_top =
            memory_set.build_initial_stack(user_stack_top, args, envs, execfn, aux, stack_limit)?;
        if memory_set.virtual_bytes() > address_space_limit || memory_set.data_bytes() > data_limit
        {
            return Err(ElfLoadError::OutOfMemory);
        }
        Ok((memory_set, actual_stack_top, entry_point))
    }
}

#[derive(Debug, Clone, Copy)]
struct LoadedElf {
    entry: usize,
    phdr: Option<usize>,
    phent: usize,
    phnum: usize,
    max_end: usize,
}
