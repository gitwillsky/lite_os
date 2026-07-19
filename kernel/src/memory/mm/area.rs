use super::*;

/// page table leaf 的物理页来源。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum MapType {
    Identical, // PA <-> VA 恒等映射
    Framed,    // 映射到分配的物理页帧
}

/// MemorySet 内部区分 VMA lifecycle 与统计语义的类别。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum VmaKind {
    System,
    Anonymous,
    Stack { top: usize },
    Elf,
    File,
    Device,
}

/// 单个 VMA 的范围、backing 与 resident frame owner。
///
/// `MemorySet::areas` 仍是 MapArea 的唯一集合 owner；字段只向 `memory::mm` sibling
/// algorithms 开放，使 cow、fault、protect、retire 继续在同一 owner seam 内原子更新。
#[derive(Debug)]
pub(crate) struct MapArea {
    /// VMA 覆盖的半开虚拟页区间。
    pub(super) vpn_range: Range<VirtualPageNumber>,
    /// 初始 data 首字节相对首页的偏移。
    pub(super) data_page_offset: usize,
    /// private resident page 的唯一 VMA-side owner index。
    pub(super) data_frames: FallibleMap<VirtualPageNumber, PrivateResident>,
    /// leaf 物理页来源。
    pub(super) map_type: MapType,
    /// 当前 semantic page permissions。
    pub(super) map_permission: MapPermission,
    /// 是否标记为全局页（G位）。仅用于内核空间映射。
    pub(super) global: bool,
    /// VMA lifecycle 与统计类别。
    pub(super) kind: VmaKind,
    /// 可选 anonymous shared backing view。
    pub(super) shared_anonymous: Option<SharedAnonymousArea>,
    /// 可选 shared-file backing 与 resident index。
    pub(super) shared_file: Option<SharedFileArea>,
    /// 可选 device extent view。
    pub(super) device: Option<DeviceArea>,
    /// 可选 private file/ELF fault source。
    pub(super) private_file: Option<PrivateFileArea>,
    /// private VMA 只声明地址范围，首次访问才分配物理页。
    pub(super) lazy_private: bool,
}

impl MapArea {
    fn byte_len(&self) -> u64 {
        let pages = self
            .vpn_range
            .end
            .as_usize()
            .checked_sub(self.vpn_range.start.as_usize())
            .expect("VMA end precedes start");
        u64::try_from(pages)
            .expect("VMA page count exceeds u64")
            .checked_mul(config::PAGE_SIZE as u64)
            .expect("VMA byte length overflow")
    }

    fn virtual_accounted_bytes(&self) -> u64 {
        if self.map_permission.contains(MapPermission::U) {
            self.byte_len()
        } else {
            0
        }
    }

    fn data_accounted_bytes(&self) -> u64 {
        if self
            .map_permission
            .contains(MapPermission::U | MapPermission::W)
            && self.shared_anonymous.is_none()
            && self.shared_file.is_none()
            && matches!(self.kind, VmaKind::Anonymous | VmaKind::Elf | VmaKind::File)
        {
            self.byte_len()
        } else {
            0
        }
    }

    /// 返回 structural publication 同步提交的 stack identity 与 RLIMIT contribution。
    ///
    /// @return 由当前 range/kind/permission/backing 计算的完整 contribution。
    pub(super) fn index_contribution(&self) -> VmaContribution {
        VmaContribution {
            start: self.vpn_range.start.as_usize(),
            stack: matches!(self.kind, VmaKind::Stack { .. }),
            virtual_bytes: self.virtual_accounted_bytes(),
            data_bytes: self.data_accounted_bytes(),
        }
    }

    /// 判断 semantic permissions 是否发布 leaf PTE。
    ///
    /// `permission` 是 VMA 当前或目标权限；返回 false 表示保留 PROT_NONE translation slot。
    pub(super) fn has_leaf_permission(permission: MapPermission) -> bool {
        permission.intersects(MapPermission::R | MapPermission::W | MapPermission::X)
    }

    /// 构造尚未提交到 MemorySet 的基础 VMA。
    ///
    /// `start_va..end_va` 是 byte address range；`map_type` 选择物理页来源，`permissions`
    /// 是 semantic leaf 权限。返回值尚未拥有可见 PTE。
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

    /// 构造 lazy private anonymous VMA。
    ///
    /// `start_va..end_va` 是 byte address range；`permissions` 是用户页权限。
    pub(super) fn anonymous(
        start_va: VirtualAddress,
        end_va: VirtualAddress,
        permissions: MapPermission,
    ) -> Self {
        let mut area = Self::new(start_va, end_va, MapType::Framed, permissions);
        area.kind = VmaKind::Anonymous;
        area.lazy_private = true;
        area
    }

    /// 构造 lazy private ELF VMA，并取得不可变 `backing` fault source。
    pub(super) fn elf(
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

    /// 构造 lazy private file VMA，并取得不可变 `backing` fault source。
    pub(super) fn file(
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

    /// 构造以 `top` 为 exclusive end 的单页初始用户栈 VMA。
    pub(super) fn stack(top: usize) -> Self {
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

    /// 设置仅供 kernel mapping 使用的 global PTE 属性并返回 area。
    pub(crate) fn set_global(mut self, global: bool) -> Self {
        self.global = global;
        self
    }

    /// 将初始化数据复制到新建 eager framed area。
    ///
    /// 数据超过映射容量、area 非 framed 或 resident owner 不完整时返回 `InvalidRange`。
    pub(super) fn copy_data(&mut self, data: &[u8]) -> Result<(), MemoryError> {
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

    /// 将 eager backing 提交到 `page_table`；lazy/file mapping 保持未 fault-in。
    ///
    /// frame、resident index 或 page-table allocation 失败时返回对应 MemoryError；caller
    /// 仍拥有 area，并负责撤销已发布的 partial PTE。
    pub(in crate::memory) fn map(
        &mut self,
        page_table: &mut PageTable,
        commit: &mut TranslationCommit,
    ) -> Result<(), MemoryError> {
        if self.device.is_some() {
            return self.map_device_area(page_table, commit);
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
        if self.map_type == MapType::Identical {
            if !Self::has_leaf_permission(self.map_permission) {
                return Err(MemoryError::InvalidRange);
            }
            let mut permissions = self.map_permission.into();
            if self.global {
                permissions |= PagePermissions::GLOBAL;
            }
            return page_table
                .map_identity_range(
                    self.vpn_range.start,
                    self.vpn_range.end,
                    permissions,
                    commit,
                )
                .map_err(Into::into);
        }
        for vpn in self.vpn_range.start.as_usize()..self.vpn_range.end.as_usize() {
            self.map_one(page_table, VirtualPageNumber::from_vpn(vpn), commit)?;
        }
        Ok(())
    }

    fn map_one(
        &mut self,
        page_table: &mut PageTable,
        vpn: VirtualPageNumber,
        commit: &mut TranslationCommit,
    ) -> Result<(), MemoryError> {
        let (ppn, frame) = match self.map_type {
            MapType::Framed => {
                let frame = try_memory_arc(alloc().ok_or(MemoryError::OutOfMemory)?)?;
                (frame.ppn, Some(frame))
            }
            MapType::Identical => unreachable!("identity areas use range leaf selection"),
        };

        let resident = frame
            .map(|frame| {
                self.data_frames
                    .try_prepare_vacant(vpn, PrivateResident::new(frame))
            })
            .transpose()
            .map_err(|_| MemoryError::OutOfMemory)?;

        if Self::has_leaf_permission(self.map_permission) {
            let mut pte_flags = self.map_permission.into();
            if self.global {
                pte_flags |= PagePermissions::GLOBAL;
            }
            page_table.map(vpn, ppn, pte_flags, commit)?;
        } else if self.map_type == MapType::Framed {
            // PROT_NONE VMA 仍由 data_frames 唯一持有物理页，但 translation slot 必须保持 reserved。
            // 若发布无 leaf access 的 entry，部分 architecture 会把它解释成 table pointer。
            page_table.reserve(vpn)?;
        } else {
            return Err(MemoryError::InvalidRange);
        }
        if let Some(resident) = resident {
            self.data_frames.commit_vacant(resident);
        }
        Ok(())
    }

    /// 按 `[start, end)` 将可保护 VMA 分成 left/middle/right，转移而不复制 resident owner。
    ///
    /// caller 已验证区间完全包含于当前 area；返回的 middle 始终存在。
    pub(super) fn partition_protectable(
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

    /// 合并相邻、同权限的 private anonymous area，并转移右侧 resident owner。
    ///
    /// caller 已通过 `anonymous_mergeable` 验证 identity 与连续性。
    pub(super) fn merge_anonymous(&mut self, mut right: Self) {
        debug_assert_eq!(self.kind, VmaKind::Anonymous);
        debug_assert_eq!(right.kind, VmaKind::Anonymous);
        debug_assert_eq!(self.vpn_range.end, right.vpn_range.start);
        debug_assert_eq!(self.map_permission, right.map_permission);
        self.vpn_range.end = right.vpn_range.end;
        self.data_frames
            .append_ordered_disjoint(&mut right.data_frames);
    }
}
