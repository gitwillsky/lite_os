//! VMA 修改：mmap、munmap、mprotect、mremap、brk、madvise、msync、mlock。
//!
//! 每个操作先完成全部校验与对象创建，再修改 VMAR 与索引；VMAR 原生处理子区间，因此拆分只改索引。

use alloc::sync::Arc;

use native_runtime::native_abi::{vm, vmo};

use super::area::{AnonymousRole, Area, AreaFlags, Backing};
use super::request::{
    MappingPlacement, MappingResourceLimits, MemoryAdvice, RemapError, RemapPlacement, RemapRequest,
    UserMappingRequest, UserMappingSource,
};
use super::{Heap, MemorySet};
use crate::memory::{MapPermission, MemoryError, MemoryObject, PAGE_SIZE};

fn page_round(length: usize) -> Result<usize, MemoryError> {
    length.checked_next_multiple_of(PAGE_SIZE).ok_or(MemoryError::OutOfMemory)
}

impl MemorySet {
    fn user_range_valid(&self, start: usize, end: usize) -> bool {
        start >= self.base.max(PAGE_SIZE) && start < end && end <= self.trampoline()
    }

    fn check_limits(&self, length: usize, data: bool, limits: MappingResourceLimits) -> Result<(), MemoryError> {
        let length = length as u64;
        if self.virtual_bytes().saturating_add(length) > limits.address_space {
            return Err(MemoryError::OutOfMemory);
        }
        if data && self.data_bytes().saturating_add(length) > limits.data {
            return Err(MemoryError::OutOfMemory);
        }
        Ok(())
    }

    /// 建立映射，返回起始地址。
    ///
    /// # Errors
    ///
    /// 范围非法（`InvalidRange`）、`FixedNoReplace` 冲突（`AddressInUse`）、共享只读文件请求写
    /// （`PermissionDenied`）、超出资源上限或内存不足（`OutOfMemory`）、页缓存失败（`Io`）。
    pub(crate) fn install_user_mapping(
        &mut self,
        request: UserMappingRequest,
        limits: MappingResourceLimits,
    ) -> Result<usize, MemoryError> {
        if request.length == 0 {
            return Err(MemoryError::InvalidRange);
        }
        let length = page_round(request.length)?;
        let start = match request.placement {
            MappingPlacement::Hint => self.place(request.address, length)?,
            MappingPlacement::Fixed | MappingPlacement::FixedNoReplace => {
                if !request.address.is_multiple_of(PAGE_SIZE) {
                    return Err(MemoryError::InvalidRange);
                }
                request.address
            }
        };
        let end = start.checked_add(length).ok_or(MemoryError::OutOfMemory)?;
        if !self.user_range_valid(start, end) {
            return Err(if request.placement == MappingPlacement::Hint {
                MemoryError::OutOfMemory
            } else {
                MemoryError::InvalidRange
            });
        }
        if request.placement == MappingPlacement::FixedNoReplace && !self.range_is_free(start, end) {
            return Err(MemoryError::AddressInUse);
        }
        let backing = match request.source {
            UserMappingSource::PrivateAnonymous => Backing::Anonymous {
                object: MemoryObject::anonymous(length, true)?,
                offset: 0,
                role: AnonymousRole::Plain,
            },
            UserMappingSource::SharedAnonymous => Backing::SharedAnonymous {
                object: MemoryObject::anonymous(length, true)?,
                offset: 0,
            },
            UserMappingSource::PrivateFile(source) => {
                let file = source.mapping.object()?;
                let object = file.private_view(source.offset, length as u64)?;
                let file_offset = source.offset;
                Backing::PrivateFile {
                    source,
                    object,
                    offset: 0,
                    file_offset,
                }
            }
            UserMappingSource::SharedFile(source) => {
                let writable = source.mapping.may_write();
                if request.permission.contains(MapPermission::W) && !writable {
                    return Err(MemoryError::PermissionDenied);
                }
                let object = source.mapping.object()?;
                let offset = source.offset;
                Backing::SharedFile {
                    source,
                    object,
                    offset,
                    writable,
                }
            }
            UserMappingSource::Device(source) => {
                let offset = (source.page_offset * PAGE_SIZE) as u64;
                if offset + length as u64 > (source.backing.pages() * PAGE_SIZE) as u64 {
                    return Err(MemoryError::InvalidRange);
                }
                Backing::Device { source, offset }
            }
            UserMappingSource::SystemV(source) => {
                if request.permission.contains(MapPermission::W) && !source.may_write {
                    return Err(MemoryError::PermissionDenied);
                }
                Backing::SystemV {
                    source: crate::memory::SharedMemoryMembership::new(source),
                    offset: 0,
                }
            }
        };
        let area = Area {
            start,
            end,
            permission: request.permission | MapPermission::U,
            backing,
            flags: if request.grows_down {
                AreaFlags::GROWS_DOWN
            } else {
                AreaFlags::empty()
            },
            name: None,
        };
        // 替换语义（MAP_FIXED）之外的任何失败都不能改变既有映射，因此限额先于移除检查。
        self.check_limits(length, area.counts_as_data(), limits)?;
        if request.placement == MappingPlacement::Fixed {
            self.remove_range(start, end)?;
        }
        self.insert(area)?;
        Ok(start)
    }

    /// 解除 `[address, address + length)` 的映射；区间内没有映射也成功。
    ///
    /// # Errors
    ///
    /// 地址未对齐、越界或长度为零（`InvalidRange`）。
    pub(crate) fn unmap_user_mapping(&mut self, address: usize, length: usize) -> Result<(), MemoryError> {
        if !address.is_multiple_of(PAGE_SIZE) || length == 0 {
            return Err(MemoryError::InvalidRange);
        }
        let end = address.checked_add(page_round(length)?).ok_or(MemoryError::InvalidRange)?;
        if !self.user_range_valid(address, end) {
            return Err(MemoryError::InvalidRange);
        }
        self.remove_range(address, end)
    }

    /// 修改权限。
    ///
    /// # Errors
    ///
    /// 区间有空洞（`OutOfMemory`，对应 ENOMEM）、后备不允许该权限（`PermissionDenied`）。
    pub(crate) fn protect_user_mapping(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
    ) -> Result<(), MemoryError> {
        if !address.is_multiple_of(PAGE_SIZE) {
            return Err(MemoryError::InvalidRange);
        }
        let end = address.checked_add(page_round(length)?).ok_or(MemoryError::OutOfMemory)?;
        if address == end {
            return Ok(());
        }
        self.require_mapped(address, end)?;
        let permission = permission | MapPermission::U;
        let mut cursor = address;
        while cursor < end {
            let area = self.area_at(cursor).expect("range checked above");
            if !area.backing.may(permission) {
                return Err(MemoryError::PermissionDenied);
            }
            cursor = area.end;
        }
        self.split_at(address)?;
        self.split_at(end)?;
        for start in self.starts_within(address, end) {
            let area = self.areas.get_mut(&start).expect("area listed above");
            self.root
                .protect(permission.vm_options(), area.start, area.len())
                .map_err(MemoryError::from_status)?;
            area.permission = permission;
        }
        Ok(())
    }

    /// 设置程序数据段结尾；返回之后的 break（失败时为未改变的旧值由调用者处理）。
    ///
    /// # Errors
    ///
    /// 区间被占用或超出资源上限（`OutOfMemory`）。
    pub(crate) fn set_program_break(
        &mut self,
        new_break: usize,
        address_space_limit: u64,
        data_limit: u64,
    ) -> Result<usize, MemoryError> {
        let heap = self.heap.clone().ok_or(MemoryError::InvalidRange)?;
        if new_break < heap.start {
            return Ok(heap.brk);
        }
        let new_mapped = new_break
            .checked_next_multiple_of(PAGE_SIZE)
            .ok_or(MemoryError::OutOfMemory)?;
        if new_mapped < heap.mapped {
            self.remove_range(new_mapped, heap.mapped)?;
        } else if new_mapped > heap.mapped {
            let grow = new_mapped - heap.mapped;
            if !self.range_is_free(heap.mapped, new_mapped) {
                return Err(MemoryError::OutOfMemory);
            }
            self.check_limits(grow, true, MappingResourceLimits::new(address_space_limit, data_limit))?;
            heap.object.set_size((new_mapped - heap.start) as u64)?;
            self.extend_heap(&heap, new_mapped)?;
        }
        let heap = self.heap.as_mut().expect("heap checked above");
        heap.mapped = new_mapped;
        heap.brk = new_break;
        Ok(new_break)
    }

    /// 把 heap 映射延伸到 `new_mapped`：接在最后一个 heap VMA 之后，或新建一个。
    fn extend_heap(&mut self, heap: &Heap, new_mapped: usize) -> Result<(), MemoryError> {
        let previous = self
            .area_at(heap.mapped.wrapping_sub(1))
            .filter(|area| {
                area.end == heap.mapped
                    && matches!(area.backing, Backing::Anonymous { role: AnonymousRole::Heap, .. })
                    && area.permission == MapPermission::U | MapPermission::R | MapPermission::W
                    && area.flags.is_empty()
            })
            .map(|area| area.start);
        let offset = (heap.mapped - heap.start) as u64;
        match previous {
            Some(start) => {
                self.root
                    .map(
                        vm::SPECIFIC | vm::READ | vm::WRITE,
                        heap.mapped - self.base,
                        heap.object.vmo(),
                        offset,
                        new_mapped - heap.mapped,
                    )
                    .map_err(MemoryError::from_status)?;
                self.areas.get_mut(&start).expect("heap area found above").end = new_mapped;
                Ok(())
            }
            None => self.insert(Area {
                start: heap.mapped,
                end: new_mapped,
                permission: MapPermission::U | MapPermission::R | MapPermission::W,
                backing: Backing::Anonymous {
                    object: heap.object.clone(),
                    offset,
                    role: AnonymousRole::Heap,
                },
                flags: AreaFlags::empty(),
                name: None,
            }),
        }
    }

    /// 建立 heap：从 `start` 开始，初始为空。
    pub(in crate::memory) fn initialize_heap(&mut self, start: usize) -> Result<(), MemoryError> {
        self.heap = Some(Heap {
            start,
            brk: start,
            mapped: start,
            object: MemoryObject::anonymous(0, true)?,
        });
        Ok(())
    }

    /// `mremap`。
    ///
    /// # Errors
    ///
    /// 见 [`RemapError`]。
    pub(crate) fn remap_user_mapping(
        &mut self,
        request: RemapRequest,
        limits: MappingResourceLimits,
    ) -> Result<usize, RemapError> {
        let RemapRequest {
            address,
            old_length,
            new_length,
            placement,
            dont_unmap,
        } = request;
        if !address.is_multiple_of(PAGE_SIZE) || new_length == 0 {
            return Err(RemapError::Invalid);
        }
        let old_length = page_round(old_length)?;
        let new_length = page_round(new_length)?;
        let old_end = address.checked_add(old_length).ok_or(RemapError::Invalid)?;
        let source = self.area_at(address).cloned().ok_or(RemapError::Unmapped)?;
        if old_end > source.end {
            return Err(RemapError::Unmapped);
        }
        if dont_unmap && (placement == RemapPlacement::InPlace || old_length != new_length) {
            return Err(RemapError::Invalid);
        }
        if old_length == 0 {
            // 共享映射的别名：只对共享后备有意义。
            if !source.backing.is_shared() || placement == RemapPlacement::InPlace {
                return Err(RemapError::Invalid);
            }
        }
        if let RemapPlacement::Fixed(target) = placement {
            let target_end = target.checked_add(new_length).ok_or(RemapError::Invalid)?;
            if !target.is_multiple_of(PAGE_SIZE)
                || !self.user_range_valid(target, target_end)
                || (target < old_end && address < target_end)
            {
                return Err(RemapError::Invalid);
            }
        }
        if new_length <= old_length && old_length != 0 && !matches!(placement, RemapPlacement::Fixed(_)) && !dont_unmap
        {
            if new_length < old_length {
                self.remove_range(address + new_length, old_end)?;
            }
            return Ok(address);
        }
        let grow = new_length.saturating_sub(old_length);
        if grow != 0 {
            self.check_limits(grow, source.counts_as_data(), limits)
                .map_err(|_| RemapError::NoMemory)?;
        }
        // 1. 原地增长：源区间在 VMA 末尾且其后空闲。
        if placement != RemapPlacement::Fixed(address)
            && !matches!(placement, RemapPlacement::Fixed(_))
            && old_length != 0
            && old_end == source.end
            && !dont_unmap
            && self.range_is_free(old_end, address + new_length)
        {
            self.ensure_backing_size(&source, (old_end - source.start) + grow)?;
            let offset = source.backing.offset() + (old_end - source.start) as u64;
            self.root
                .map(
                    vm::SPECIFIC | source.permission.vm_options(),
                    old_end - self.base,
                    source.backing.object().vmo(),
                    offset,
                    grow,
                )
                .map_err(MemoryError::from_status)?;
            self.areas.get_mut(&source.start).expect("source area").end = address + new_length;
            return Ok(address);
        }
        if placement == RemapPlacement::InPlace {
            return Err(RemapError::NoMemory);
        }
        // 2. 搬迁：在新地址映射同一后备对象的对应部分，再处理旧区间。
        let target = match placement {
            RemapPlacement::Fixed(target) => {
                self.remove_range(target, target + new_length)?;
                target
            }
            _ => self.place(0, new_length).map_err(|_| RemapError::NoMemory)?,
        };
        let delta = (address - source.start) as u64;
        self.ensure_backing_size(&source, (address - source.start) + new_length)?;
        let moved = Area {
            start: target,
            end: target + new_length,
            permission: source.permission,
            backing: source.backing.advanced(delta),
            flags: source.flags,
            name: source.name.clone(),
        };
        self.insert(moved)?;
        if old_length != 0 {
            self.remove_range(address, old_end)?;
            if dont_unmap {
                // 旧区间保留为同权限的新空匿名映射（Linux MREMAP_DONTUNMAP）。
                self.insert(Area {
                    start: address,
                    end: old_end,
                    permission: source.permission,
                    backing: Backing::Anonymous {
                        object: MemoryObject::anonymous(old_length, true)?,
                        offset: 0,
                        role: AnonymousRole::Plain,
                    },
                    flags: AreaFlags::empty(),
                    name: None,
                })?;
            }
        }
        Ok(target)
    }

    /// 保证 VMA 起点之后 `length` 字节都落在后备对象内（匿名对象按需扩大）。
    fn ensure_backing_size(&self, area: &Area, length: usize) -> Result<(), MemoryError> {
        let needed = area.backing.offset() + length as u64;
        let object = area.backing.object();
        if object.size()? >= needed {
            return Ok(());
        }
        match area.backing {
            Backing::Anonymous { .. } | Backing::SharedAnonymous { .. } => object.set_size(needed),
            // 文件映射越过文件尾的部分访问时得到 SIGBUS；设备与段的大小固定。
            Backing::PrivateFile { .. } | Backing::SharedFile { .. } => Ok(()),
            _ => Err(MemoryError::InvalidRange),
        }
    }

    /// `madvise`。
    ///
    /// # Errors
    ///
    /// 区间有空洞（`OutOfMemory`）、锁定页上的 `DONTNEED`（`InvalidRange`）。
    pub(crate) fn advise_user_mapping(
        &mut self,
        address: usize,
        length: usize,
        advice: MemoryAdvice,
    ) -> Result<(), MemoryError> {
        if !address.is_multiple_of(PAGE_SIZE) {
            return Err(MemoryError::InvalidRange);
        }
        let end = address.checked_add(page_round(length)?).ok_or(MemoryError::InvalidRange)?;
        if address == end {
            return Ok(());
        }
        self.require_mapped(address, end)?;
        let flag_change = match advice {
            MemoryAdvice::DontFork => Some((AreaFlags::DONT_FORK, true)),
            MemoryAdvice::DoFork => Some((AreaFlags::DONT_FORK, false)),
            MemoryAdvice::WipeOnFork => Some((AreaFlags::WIPE_ON_FORK, true)),
            MemoryAdvice::KeepOnFork => Some((AreaFlags::WIPE_ON_FORK, false)),
            MemoryAdvice::DontDump => Some((AreaFlags::DONT_DUMP, true)),
            MemoryAdvice::DoDump => Some((AreaFlags::DONT_DUMP, false)),
            _ => None,
        };
        if let Some((flag, set)) = flag_change {
            if flag == AreaFlags::WIPE_ON_FORK {
                // Linux 只允许私有匿名映射使用 WIPEONFORK。
                let mut cursor = address;
                while cursor < end {
                    let area = self.area_at(cursor).expect("range checked above");
                    if !matches!(area.backing, Backing::Anonymous { .. }) {
                        return Err(MemoryError::InvalidRange);
                    }
                    cursor = area.end;
                }
            }
            self.split_at(address)?;
            self.split_at(end)?;
            for start in self.starts_within(address, end) {
                self.areas.get_mut(&start).expect("area listed above").flags.set(flag, set);
            }
            return Ok(());
        }
        if !matches!(advice, MemoryAdvice::DontNeed | MemoryAdvice::Free) {
            // 访问模式提示与预取：内核按需提交页，没有需要改变的状态。
            return Ok(());
        }
        let mut cursor = address;
        while cursor < end {
            let area = self.area_at(cursor).expect("range checked above").clone();
            let range_end = area.end.min(end);
            let offset = area.backing.offset() + (cursor - area.start) as u64;
            match &area.backing {
                // 私有页丢弃后重新读到零（匿名）或文件当前内容（私有文件映射的写时复制页被撤销）。
                Backing::Anonymous { object, .. } | Backing::PrivateFile { object, .. } => {
                    object
                        .vmo()
                        .op_range(vmo::OP_DECOMMIT, offset, (range_end - cursor) as u64)
                        .map_err(MemoryError::from_status)?;
                }
                // 共享映射的内容属于共享对象：DONTNEED 只撤销本进程的翻译，内容保持。
                _ => {}
            }
            cursor = range_end;
        }
        Ok(())
    }

    /// `msync`：把共享文件映射中的脏页写回。
    ///
    /// # Errors
    ///
    /// 区间有空洞（`OutOfMemory`）、写回失败（`Io`）。
    pub(crate) fn sync_shared_mapping(&self, address: usize, length: usize, writeback: bool) -> Result<(), MemoryError> {
        if !address.is_multiple_of(PAGE_SIZE) {
            return Err(MemoryError::InvalidRange);
        }
        let end = address.checked_add(page_round(length)?).ok_or(MemoryError::OutOfMemory)?;
        self.require_mapped(address, end)?;
        if !writeback {
            return Ok(());
        }
        let mut cursor = address;
        while cursor < end {
            let area = self.area_at(cursor).expect("range checked above");
            let range_end = area.end.min(end);
            if let Backing::SharedFile { source, offset, .. } = &area.backing {
                let file_offset = offset + (cursor - area.start) as u64;
                source
                    .mapping
                    .sync_range(file_offset, (range_end - cursor) as u64)
                    .map_err(MemoryError::from)?;
            }
            cursor = range_end;
        }
        Ok(())
    }

    /// `shmdt`：`address` 必须是一次挂接的起点；移除属于同一次挂接的全部片段（拆分产生的 VMA）。
    ///
    /// # Errors
    ///
    /// `address` 不是 SysV 挂接的起点（`InvalidRange`）。
    pub(crate) fn detach_shared_memory(&mut self, address: usize) -> Result<(), MemoryError> {
        let lifetime = match self.area_at(address) {
            Some(Area {
                start,
                backing: Backing::SystemV { source, .. },
                ..
            }) if *start == address => source.source.lifetime.clone(),
            _ => return Err(MemoryError::InvalidRange),
        };
        let ranges: alloc::vec::Vec<(usize, usize)> = self
            .areas
            .iter()
            .filter(|(_, area)| {
                matches!(&area.backing, Backing::SystemV { source, .. } if Arc::ptr_eq(&source.source.lifetime, &lifetime))
            })
            .map(|(_, area)| (area.start, area.end))
            .collect();
        for (start, end) in ranges {
            self.remove_range(start, end)?;
        }
        Ok(())
    }
}
