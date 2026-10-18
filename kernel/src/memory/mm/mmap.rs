//! 用户地址空间的建立、拆分与回收：`mmap`、`munmap`、`mprotect` 的 VMA 层。
//!
//! # VMA 表
//!
//! 进程的用户地址空间是一组 VMA：连续、权限与后备都相同的页区间。它们存放在以**起始页号**为键的有序表
//! （`FallibleMap`）里，所以“某个地址落在哪个 VMA”是一次 `floor` 查找，“按地址顺序走一遍”是一次有序遍历。
//!
//! # 按需分配
//!
//! 私有匿名映射和共享文件映射只在表里登记一个 VMA，不分配物理页，也不碰页表（`MapArea::map` 对
//! `lazy_private` 与 `shared_file` 直接返回）。页在第一次被访问、触发缺页时才出现（见 `cow.rs` 与
//! `mmap/fault.rs`）。设备映射与共享匿名映射则在登记时就建立页表。
//!
//! # 选址：首次适应
//!
//! `find_free_user_range` 从 `MMAP_BASE` 起沿 VMA 有序表走一遍，维护一个候选起点 `start`：遇到一个 VMA，
//! 如果 `[start, start + n)` 完全放得进它前面的空洞就用；否则把 `start` 移到这个 VMA 的末尾继续。代价与
//! VMA 个数成正比。调用者给了地址提示时先试提示地址，空闲就用；`fixed_noreplace` 冲突时返回
//! `AddressInUse`，绝不替换既有映射。
//!
//! # 合并与拆分
//!
//! - **合并**：新建的匿名映射会和权限相同的相邻匿名 VMA 合并（`merge_anonymous_neighbors`），避免大量小
//!   映射把 VMA 表撑大。
//! - **拆分**：`munmap` 与 `mprotect` 的范围常常落在某个 VMA 中间，于是一个 VMA 被切成
//!   left / middle / right 三段，只有 middle 受影响。
//!
//! # 原则：先预留，再修改
//!
//! 拆分意味着要新建 VMA 节点，而分配可能失败。`prepare_user_unmap` 与 `protect_user_mapping` 都按同一个
//! 顺序做事：
//!
//! 1. 校验范围，收集受影响的 VMA。
//! 2. 数出会**真正保留**多少 left/right 段（整段删除不需要新节点），用 `try_reserve_node` 把这些节点和回收
//!    用的 `Vec` 全部预留好。
//! 3. 做所有可能失败或要等待的事：共享文件映射的写回、pin 住常驻页、为将发布的页预留页表路径。
//! 4. 最后才修改：此后只剩不分配的指针操作。
//!
//! 因此任何失败路径都保证 VMA 表和页表原封不动，不会留下“拆了一半”的地址空间。这就是
//! `fallible_tree` 的“准备/提交分离”在大模块里的样子。
//!
//! # munmap 为什么不直接释放
//!
//! `prepare_user_unmap` 摘下 VMA 后返回 `PreparedAreaRetirement`，由调用者在**释放 MM 锁之后**完成 fence 并
//! 释放物理页（原因见 `shootdown.rs`：必须先让所有 CPU 忘掉旧翻译）。
//!
//! # 阅读顺序
//!
//! `find_free_user_range` → `map_anonymous` → `merge_anonymous_neighbors` → `prepare_user_unmap` →
//! `mmap/protection.rs`（同样的拆分，改的是权限）。

use super::*;

mod advice;
mod anonymous_shared;
mod device;
mod fault;
mod installation;
mod protection;
mod remap;
pub(crate) use remap::{RemapError, RemapPlacement, RemapRequest};
mod shared_write;

/// 一次用户页访问 fault 的领域结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageFaultOutcome {
    /// 请求的访问权限已经由 live leaf PTE 满足，原指令可直接重试。
    Handled,
    /// 地址不属于允许该访问的用户 VMA；cause 区分未映射与权限拒绝。
    SegmentationFault(SegmentationCause),
    /// file mapping 地址属于 VMA，但已越过 backing object 的有效范围。
    BusError,
}

impl MemorySet {
    fn find_free_user_range(
        &self,
        first: VirtualPageNumber,
        page_count: usize,
    ) -> Option<Range<VirtualPageNumber>> {
        let user_end = config::USER_ADDRESS_END / config::PAGE_SIZE;
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

    /// 建立按需分配的 anonymous private 用户映射。
    ///
    /// # Parameters
    ///
    /// - `address`: 零表示由内核选址；非零是 page-aligned hint 或 fixed-noreplace 地址。
    /// - `length`: 非零字节长度，向上取整到整页。
    /// - `permission`: 用户页权限；必须含 U，允许 PROT_NONE 与 Linux W+X 映射。
    /// - `fixed_noreplace`: 为真时地址冲突返回 `AddressInUse`，不替换既有 VMA。
    ///
    /// # Returns
    ///
    /// 成功返回 page-aligned 起始地址；任何失败都不改变页表或 VMA 表。
    pub(crate) fn map_anonymous(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        address_space_limit: u64,
        data_limit: u64,
    ) -> Result<usize, MemoryError> {
        if length == 0
            || !permission.contains(MapPermission::U)
            || (fixed_noreplace && (address == 0 || !VirtualAddress::from(address).is_aligned()))
        {
            return Err(MemoryError::InvalidRange);
        }
        let page_count = length
            .checked_add(config::PAGE_SIZE - 1)
            .ok_or(MemoryError::InvalidRange)?
            / config::PAGE_SIZE;
        self.ensure_resource_capacity(
            page_count as u64 * config::PAGE_SIZE as u64,
            address_space_limit,
            permission.contains(MapPermission::W).then_some(data_limit),
        )?;
        let hinted_start = VirtualAddress::from(address).floor();
        let hinted_end = hinted_start
            .as_usize()
            .checked_add(page_count)
            .map(VirtualPageNumber::from_vpn)
            .ok_or(MemoryError::InvalidRange)?;
        let user_end_vpn = config::USER_ADDRESS_END / config::PAGE_SIZE;
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
        self.merge_anonymous_neighbors(range.start);
        Ok(start_address)
    }

    /// 建立按需 fault 的 file-backed private 映射。
    pub(crate) fn map_private_file(
        &mut self,
        address: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        file: FileMappingSource,
        limits: MappingResourceLimits,
    ) -> Result<usize, MemoryError> {
        let FileMappingSource { mapping, pages } = file;
        if !permission.contains(MapPermission::U)
            || (fixed_noreplace && (address == 0 || !VirtualAddress::from(address).is_aligned()))
        {
            return Err(MemoryError::InvalidRange);
        }
        let page_count = usize::try_from(pages.count()).map_err(|_| MemoryError::InvalidRange)?;
        self.ensure_resource_capacity(
            page_count as u64 * config::PAGE_SIZE as u64,
            limits.address_space,
            permission.contains(MapPermission::W).then_some(limits.data),
        )?;
        let hinted_start = VirtualAddress::from(address).floor();
        let hinted_end = hinted_start
            .as_usize()
            .checked_add(page_count)
            .map(VirtualPageNumber::from_vpn)
            .ok_or(MemoryError::InvalidRange)?;
        let user_end = config::USER_ADDRESS_END / config::PAGE_SIZE;
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
        let backing = PrivateFileArea::cached_file(mapping, start, pages);
        self.push(
            MapArea::file(start.into(), end.into(), permission, backing),
            None,
        )?;
        Ok(start)
    }

    /// 建立 lazy file-backed shared mapping；page cache 是所有 resident page 的唯一 owner。
    pub(crate) fn map_shared_file(
        &mut self,
        address: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        file: FileMappingSource,
        address_space_limit: u64,
    ) -> Result<usize, MemoryError> {
        let FileMappingSource { mapping, pages } = file;
        if !permission.contains(MapPermission::U)
            || (fixed_noreplace && (address == 0 || !VirtualAddress::from(address).is_aligned()))
        {
            return Err(MemoryError::InvalidRange);
        }
        if permission.contains(MapPermission::W) {
            if !mapping.may_write() {
                return Err(MemoryError::PermissionDenied);
            }
            if !mapping.writable() {
                return Err(MemoryError::TextBusy);
            }
        }
        let page_count = usize::try_from(pages.count()).map_err(|_| MemoryError::InvalidRange)?;
        self.ensure_resource_capacity(
            page_count as u64 * config::PAGE_SIZE as u64,
            address_space_limit,
            None,
        )?;
        let hinted_start = VirtualAddress::from(address).floor();
        let hinted_end = hinted_start
            .as_usize()
            .checked_add(page_count)
            .map(VirtualPageNumber::from_vpn)
            .ok_or(MemoryError::InvalidRange)?;
        let user_end = config::USER_ADDRESS_END / config::PAGE_SIZE;
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
            MapArea::shared_file(start.into(), end.into(), permission, mapping, pages),
            None,
        )?;
        Ok(start)
    }

    pub(crate) fn sync_shared_mapping(
        &self,
        address: usize,
        length: usize,
        writeback: bool,
    ) -> Result<(), MemoryError> {
        let range = Self::checked_page_range(address, length)?;
        let mut covered = range.start;
        for area in self
            .areas
            .values()
            .filter(|area| range.start < area.vpn_range.end && area.vpn_range.start < range.end)
        {
            if area.vpn_range.start > covered {
                return Err(MemoryError::InvalidRange);
            }
            let start = range.start.max(area.vpn_range.start);
            let end = range.end.min(area.vpn_range.end);
            if writeback && let Some(shared) = &area.shared_file {
                shared.sync_vma_range(area.vpn_range.start, start, end)?;
            }
            covered = covered.max(area.vpn_range.end);
        }
        (covered >= range.end)
            .then_some(())
            .ok_or(MemoryError::InvalidRange)
    }

    pub(crate) fn invalidate_shared_file(&mut self, id: SharedFileId, size: u64) {
        let memory = revoke_and_commit(self, |memory, commit| {
            memory.revoke_invalidated_file_translations(id, size, commit);
        })
        .expect("platform TLB synchronization failed after truncate invalidation");
        memory.release_invalidated_file_residents(id, size);
    }

    fn invalidated_file_starts(
        area: &MapArea,
        id: SharedFileId,
        size: u64,
    ) -> (Option<VirtualPageNumber>, Option<VirtualPageNumber>) {
        let start = area.vpn_range.start;
        let shared = area
            .shared_file
            .as_ref()
            .filter(|shared| shared.mapping.id() == id)
            .and_then(|shared| shared.pages.prefix_before(size))
            .and_then(|offset| usize::try_from(offset).ok())
            .and_then(|offset| start.as_usize().checked_add(offset))
            .map(VirtualPageNumber::from_vpn);
        let private = area
            .private_file
            .as_ref()
            .and_then(|private| private.first_stale_page(start, id, size));
        (shared, private)
    }

    fn revoke_invalidated_file_translations(
        &mut self,
        id: SharedFileId,
        size: u64,
        commit: &mut TranslationCommit,
    ) {
        let page_table = &mut self.page_table;
        self.areas.for_each_mut(|_, area| {
            let (shared_start, private_start) = Self::invalidated_file_starts(area, id, size);
            if let Some(first_stale) = shared_start
                && let Some(shared) = &area.shared_file
            {
                for (&vpn, _) in shared.resident.iter_from(&first_stale) {
                    let _ = page_table.unmap(vpn, commit);
                }
            }
            if let Some(first_stale) = private_start {
                for (&vpn, _) in area.data_frames.iter_from(&first_stale) {
                    let _ = page_table.unmap(vpn, commit);
                }
            }
        });
    }

    fn release_invalidated_file_residents(&mut self, id: SharedFileId, size: u64) {
        self.areas.for_each_mut(|_, area| {
            let (shared_start, private_start) = Self::invalidated_file_starts(area, id, size);
            if let Some(first_stale) = shared_start
                && let Some(shared) = &mut area.shared_file
            {
                drop(shared.resident.split_off(&first_stale));
            }
            if let Some(first_stale) = private_start {
                // Linux truncate 的 even_cows 语义撤销 EOF 外 private residency，包括
                // operation snapshot 后、truncate callback 取得 mm lock 前发布的页。
                drop(area.data_frames.split_off(&first_stale));
            }
        });
    }

    fn overlapping_mmap_keys(
        &self,
        start: VirtualPageNumber,
        end: VirtualPageNumber,
    ) -> Result<Vec<VirtualPageNumber>, MemoryError> {
        let mut keys = Vec::new();
        let first = self.areas.floor(&start).map_or(start, |(key, _)| *key);
        for (key, area) in self
            .areas
            .iter_from(&first)
            .take_while(|(key, _)| **key < end)
        {
            if start < area.vpn_range.end {
                if area.kind == VmaKind::System {
                    return Err(MemoryError::PermissionDenied);
                }
                keys.try_reserve(1).map_err(|_| MemoryError::OutOfMemory)?;
                keys.push(*key);
            }
        }
        Ok(keys)
    }

    fn merge_anonymous_neighbors(&mut self, key: VirtualPageNumber) {
        let Some(mut current_key) = self
            .areas
            .floor(&key)
            .filter(|(_, area)| key < area.vpn_range.end)
            .map(|(key, _)| *key)
            .or_else(|| self.areas.ceiling(&key).map(|(key, _)| *key))
        else {
            return;
        };

        if let Some(left_key) = self
            .areas
            .predecessor(&current_key)
            .filter(|(_, left)| left.anonymous_mergeable(&self.areas[&current_key]))
            .map(|(key, _)| *key)
        {
            self.merge_anonymous_pair(left_key, current_key);
            current_key = left_key;
        }
        while let Some(right_key) = self
            .areas
            .successor(&current_key)
            .filter(|(_, right)| self.areas[&current_key].anonymous_mergeable(right))
            .map(|(key, _)| *key)
        {
            self.merge_anonymous_pair(current_key, right_key);
        }
    }

    fn merge_anonymous_pair(&mut self, left_key: VirtualPageNumber, right_key: VirtualPageNumber) {
        let mut left = self
            .take_area_entry(&left_key)
            .expect("merge left VMA must remain present");
        let right = self
            .remove_area(&right_key)
            .expect("merge right VMA must remain present");
        left.value_mut().merge_anonymous(right);
        self.commit_area(left);
    }

    /// 解除任意用户 VMA（包括 ELF 与 grow-down stack）；未映射洞按 Linux 语义忽略。
    ///
    /// # Parameters
    ///
    /// - `address`: page-aligned 起始地址。
    /// - `length`: 非零字节长度，向上取整到整页。
    ///
    /// # Returns
    ///
    /// 成功返回尚未完成 fence 的 retirement；caller 释放 MM lock 后必须同步并释放。
    /// 内部 System VMA 或 preparation 错误保持全部映射不变。
    pub(crate) fn prepare_user_unmap(
        &mut self,
        address: usize,
        length: usize,
    ) -> Result<PreparedAreaRetirement, MemoryError> {
        let range = Self::checked_page_range(address, length)?;
        let keys = self.overlapping_mmap_keys(range.start, range.end)?;
        // 1. 只计算真正保留的 left/right segment，避免完整删除也临时占用两枚节点。
        let slot_count = keys.iter().try_fold(0usize, |count, key| {
            let area = &self.areas[key];
            let cut_start = range.start.max(area.vpn_range.start);
            let cut_end = range.end.min(area.vpn_range.end);
            count
                .checked_add(usize::from(area.vpn_range.start < cut_start))
                .and_then(|count| count.checked_add(usize::from(cut_end < area.vpn_range.end)))
                .ok_or(MemoryError::OutOfMemory)
        })?;
        // 2. 在 sync、PTE 撤销与 VMA removal 前完成全部可失败分配。
        let mut segment_slots = Vec::new();
        segment_slots
            .try_reserve_exact(slot_count)
            .map_err(|_| MemoryError::OutOfMemory)?;
        for _ in 0..slot_count {
            segment_slots.push(
                FallibleMap::<VirtualPageNumber, MapArea>::try_reserve_node()
                    .map_err(|_| MemoryError::OutOfMemory)?,
            );
        }
        // 被切除的 area 必须在 remote fence 完成前保留全部 frame/device/writer owner。
        // 缺少该 preflight 会在 PTE 已撤销后因 retire Vec OOM 无法安全保活 owner。
        let mut retired_areas = Vec::new();
        retired_areas
            .try_reserve_exact(keys.len())
            .map_err(|_| MemoryError::OutOfMemory)?;
        // 3. 从这里开始只有 writeback/PTE 领域错误，VMA segment publication 不再分配。
        for key in &keys {
            let area = &self.areas[key];
            let start = range.start.max(area.vpn_range.start);
            let end = range.end.min(area.vpn_range.end);
            if let Some(shared) = &area.shared_file {
                shared.sync_vma_range(area.vpn_range.start, start, end)?;
            }
        }
        let mut segment_slots = segment_slots.into_iter();
        for key in keys {
            let area = self.remove_area(&key).unwrap();
            let cut_start = range.start.max(area.vpn_range.start);
            let cut_end = range.end.min(area.vpn_range.end);
            let (left, middle, right) = area.partition_protectable(cut_start, cut_end);
            if let Some(left) = left {
                let slot = segment_slots.next().expect("preflighted left VMA slot");
                self.commit_area(slot.fill(left.vpn_range.start, left));
            }
            if let Some(right) = right {
                let slot = segment_slots.next().expect("preflighted right VMA slot");
                self.commit_area(slot.fill(right.vpn_range.start, right));
            }
            retired_areas.push(middle);
        }
        assert!(
            self.range_is_free(range.start, range.end),
            "preflighted munmap range remained published"
        );
        Ok(PreparedAreaRetirement::batch(
            retired_areas,
            &mut self.page_table,
        ))
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
        let user_end = config::USER_ADDRESS_END;
        if end > user_end {
            return Err(MemoryError::InvalidRange);
        }
        Ok(VirtualAddress::from(address).floor()..VirtualAddress::from(end).floor())
    }
}
