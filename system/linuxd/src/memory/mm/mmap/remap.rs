//! `mremap`：缩小、原地增长、搬迁（`MAYMOVE`/`FIXED`/`DONTUNMAP`）与共享映射别名（`old_size == 0`）。
//!
//! # 搬迁转移 owner，不复制页
//!
//! 搬迁把源范围从 VMA 里切出来（沿用 munmap 的 left/middle/right 切分），撤销 middle 的旧 PTE，
//! 把它整体 `rebase` 到新地址再发布。resident 页、共享 backing 与文件范围都随 middle 移动；
//! 新地址的 PTE 不预先建立，首次访问时由缺页路径按已有 owner 重新安装（fork 出来的子进程
//! 就处于同样的“页已驻留、PTE 尚缺”状态）。旧地址的 stale 翻译由返回的 fence 封闭。
//!
//! # 原则：先校验、先预留、最后修改
//!
//! 所有可失败的检查与分配（范围、资源上限、节点、退休 Vec）都在第一次修改前完成。
//! `FIXED` 目标的撤销是第一个破坏性步骤，之后只剩不分配的指针操作；即使别名安装在它之后失败，
//! 目标也只是已被解除映射，与 Linux 一致。fence 总是随结果一起返回，由 caller 在释放 MM lock 后同步。

use super::*;

/// 目标位置策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemapPlacement {
    /// 只能原地调整（没有 `MREMAP_MAYMOVE`）。
    InPlace,
    /// 原地放不下时由内核选址搬迁（`MREMAP_MAYMOVE`）。
    MayMove,
    /// 搬到指定的 page-aligned 地址，先解除该处既有映射（`MREMAP_MAYMOVE | MREMAP_FIXED`）。
    Fixed(usize),
}

/// 一次 `mremap` 的完整输入。
#[derive(Debug, Clone, Copy)]
pub(crate) struct RemapRequest {
    pub(crate) address: usize,
    pub(crate) old_length: usize,
    pub(crate) new_length: usize,
    pub(crate) placement: RemapPlacement,
    /// `MREMAP_DONTUNMAP`：搬迁后旧范围保留为新的空匿名映射。
    pub(crate) dont_unmap: bool,
}

/// `mremap` 的失败分类；由 syscall 层翻译成 errno。
#[derive(Debug)]
pub(crate) enum RemapError {
    /// 源范围没有被单个可重映射的 VMA 完整覆盖（EFAULT）。
    Unmapped,
    /// 参数组合非法（EINVAL）。
    Invalid,
    /// 原地放不下且不允许搬迁，或资源上限不足（ENOMEM）。
    NoMemory,
    Memory(MemoryError),
}

/// 结果与待同步 fence 的唯一 owner；caller 必须在释放 MM lock 后调用 `synchronize`。
pub(crate) struct PreparedUserRemap {
    result: Result<usize, RemapError>,
    destination: Option<PreparedAreaRetirement>,
    source: Option<PreparedAreaRetirement>,
}

impl PreparedUserRemap {
    /// 完成全部 TLB fence 并释放被解除的 backing，然后返回 remap 结果。
    pub(crate) fn synchronize(self) -> Result<usize, RemapError> {
        if let Some(destination) = self.destination {
            destination.synchronize();
        }
        if let Some(source) = self.source {
            source.synchronize();
        }
        self.result
    }
}

impl MemorySet {
    /// 在一次 MM lock 内完成 `mremap` 的全部 VMA 与 PTE 变更。
    ///
    /// # Parameters
    ///
    /// - `request`: 源范围、目标长度、放置策略与 `DONTUNMAP`。
    /// - `limits`: 本进程的 RLIMIT_AS/RLIMIT_DATA。
    ///
    /// # Returns
    ///
    /// 成功时是调整后的起始地址；失败时源映射保持原样（`FIXED` 目标可能已被解除）。
    pub(crate) fn prepare_user_remap(
        &mut self,
        request: RemapRequest,
        limits: MappingResourceLimits,
    ) -> PreparedUserRemap {
        let mut destination = None;
        let mut source = None;
        let result = self.remap(request, limits, &mut destination, &mut source);
        PreparedUserRemap {
            result,
            destination,
            source,
        }
    }

    fn remap(
        &mut self,
        request: RemapRequest,
        limits: MappingResourceLimits,
        destination_fence: &mut Option<PreparedAreaRetirement>,
        source_fence: &mut Option<PreparedAreaRetirement>,
    ) -> Result<usize, RemapError> {
        let page = config::PAGE_SIZE;
        let RemapRequest {
            address,
            old_length,
            new_length,
            placement,
            dont_unmap,
        } = request;
        let round = |length: usize| {
            length
                .checked_add(page - 1)
                .map(|value| value / page * page)
                .ok_or(RemapError::Invalid)
        };
        let (old_len, new_len) = (round(old_length)?, round(new_length)?);
        if address == 0
            || !VirtualAddress::from(address).is_aligned()
            || new_len == 0
            || dont_unmap && (old_len != new_len || placement == RemapPlacement::InPlace)
        {
            return Err(RemapError::Invalid);
        }
        let user_end = config::USER_ADDRESS_END;
        let old_end = address
            .checked_add(old_len)
            .filter(|end| *end <= user_end)
            .ok_or(RemapError::Invalid)?;
        let old_start_vpn = VirtualAddress::from(address).floor();
        let old_end_vpn = VirtualAddress::from(old_end).floor();

        // 1. 源范围必须被单个 VMA 完整覆盖；stack 与 System VMA 的大小由各自 owner 管理。
        let area_key = self
            .areas
            .floor(&old_start_vpn)
            .filter(|(_, area)| old_start_vpn < area.vpn_range.end)
            .map(|(key, _)| *key)
            .ok_or(RemapError::Unmapped)?;
        let area = &self.areas[&area_key];
        if old_end_vpn > area.vpn_range.end || matches!(area.kind, VmaKind::System | VmaKind::Stack)
        {
            return Err(RemapError::Unmapped);
        }
        let area_end_vpn = area.vpn_range.end;
        let permission = area.map_permission;
        let contribution = area.index_contribution();
        let shared = area.shared_anonymous.is_some() || area.shared_file.is_some();
        let private_anonymous = area.kind == VmaKind::Anonymous
            && area.lazy_private
            && !shared
            && area.device.is_none()
            && area.private_file.is_none();
        // old_len == 0 在共享映射上创建别名；私有映射没有可共享的 backing。
        if old_len == 0 && !shared || dont_unmap && !private_anonymous {
            return Err(RemapError::Invalid);
        }
        // 别名需要新的地址，没有 MREMAP_MAYMOVE 时无处可放。
        if old_len == 0 && placement == RemapPlacement::InPlace {
            return Err(RemapError::NoMemory);
        }
        let growing = new_len > old_len;
        let extra_pages = new_len.saturating_sub(old_len) / page;
        if growing && !area.can_extend_tail(extra_pages) {
            // device 不可增长（Linux VM_DONTEXPAND 返回 EFAULT）；其余只会因文件偏移溢出失败。
            return Err(if area.device.is_some() {
                RemapError::Unmapped
            } else {
                RemapError::NoMemory
            });
        }
        let alias_pages = match (&area.shared_file, old_len == 0) {
            (Some(file), true) => {
                let relative = (old_start_vpn.as_usize() - area.vpn_range.start.as_usize()) as u64;
                Some(
                    file.pages
                        .subrange(relative, 1)
                        .and_then(|first| first.extended((new_len / page - 1) as u64))
                        .ok_or(RemapError::NoMemory)?,
                )
            }
            _ => None,
        };

        // 2. 不搬迁的情形：缩小、不变、原地增长。
        if !matches!(placement, RemapPlacement::Fixed(_)) && !dont_unmap && old_len != 0 {
            if new_len < old_len {
                let tail = self
                    .prepare_user_unmap(address + new_len, old_len - new_len)
                    .map_err(RemapError::Memory)?;
                *source_fence = Some(tail);
                return Ok(address);
            }
            if new_len == old_len {
                return Ok(address);
            }
            // 只有源末尾恰好是 VMA 末尾时，后面才可能是空洞。
            let new_end_vpn = VirtualPageNumber::from_vpn(old_end_vpn.as_usize() + extra_pages);
            if old_end_vpn == area_end_vpn
                && new_end_vpn.as_usize() <= user_end / page
                && self.range_is_free(old_end_vpn, new_end_vpn)
            {
                self.ensure_resource_capacity(
                    (new_len - old_len) as u64,
                    limits.address_space,
                    (contribution.data_bytes != 0).then_some(limits.data),
                )
                .map_err(|_| RemapError::NoMemory)?;
                let mut entry = self
                    .take_area_entry(&area_key)
                    .expect("remap source VMA must remain present");
                entry.value_mut().extend_tail(extra_pages);
                self.commit_area(entry);
                self.merge_anonymous_neighbors(area_key);
                return Ok(address);
            }
            if placement == RemapPlacement::InPlace {
                return Err(RemapError::NoMemory);
            }
        }

        // 3. 搬迁：确定目标起点。
        let new_pages = new_len / page;
        let destination_start = match placement {
            RemapPlacement::Fixed(target) => {
                let target_end = target
                    .checked_add(new_len)
                    .filter(|end| *end <= user_end)
                    .ok_or(RemapError::Invalid)?;
                // 与 Linux 一致：目标与源（按 old_len）相交一律 EINVAL。
                if target == 0
                    || !VirtualAddress::from(target).is_aligned()
                    || target < old_end && address < target_end
                {
                    return Err(RemapError::Invalid);
                }
                VirtualAddress::from(target).floor()
            }
            _ => {
                self.find_free_user_range(VirtualAddress::from(Self::MMAP_BASE).floor(), new_pages)
                    .ok_or(RemapError::NoMemory)?
                    .start
            }
        };
        let destination_range = destination_start
            ..VirtualPageNumber::from_vpn(destination_start.as_usize() + new_pages);

        // 4. 资源：搬迁本身不改变 usage，只有增长部分、DONTUNMAP 保留的旧范围与别名是净新增。
        let kept_bytes = if dont_unmap { old_len as u64 } else { 0 };
        let extra_bytes = if old_len == 0 {
            new_len as u64
        } else {
            (new_len.saturating_sub(old_len)) as u64
        } + kept_bytes;
        let extra_data = if contribution.data_bytes != 0 || dont_unmap {
            extra_bytes
        } else {
            0
        };
        let (removed_virtual, removed_data) = if matches!(placement, RemapPlacement::Fixed(_)) {
            self.replaced_usage(&destination_range)
        } else {
            (0, 0)
        };
        if self
            .virtual_bytes()
            .checked_sub(removed_virtual)
            .and_then(|current| current.checked_add(extra_bytes))
            .is_none_or(|total| total > limits.address_space)
            || extra_data != 0
                && self
                    .data_bytes()
                    .checked_sub(removed_data)
                    .and_then(|current| current.checked_add(extra_data))
                    .is_none_or(|total| total > limits.data)
        {
            return Err(RemapError::NoMemory);
        }

        // 5. 预留：最多 left、right、目标、DONTUNMAP 保留区间四个节点，加一个被丢弃尾部的退休 Vec。
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(4)
            .map_err(|_| RemapError::Memory(MemoryError::OutOfMemory))?;
        for _ in 0..4 {
            slots.push(
                FallibleMap::<VirtualPageNumber, MapArea>::try_reserve_node()
                    .map_err(|_| RemapError::Memory(MemoryError::OutOfMemory))?,
            );
        }
        let mut discarded_areas = Vec::new();
        discarded_areas
            .try_reserve_exact(1)
            .map_err(|_| RemapError::Memory(MemoryError::OutOfMemory))?;

        // 6. 第一个破坏性步骤：解除 FIXED 目标。此后只剩不分配的操作（别名的 push 除外）。
        if let RemapPlacement::Fixed(target) = placement {
            *destination_fence = Some(
                self.prepare_user_unmap(target, new_len)
                    .map_err(RemapError::Memory)?,
            );
        }
        let destination_address = usize::from(VirtualAddress::from(destination_start));
        // 目标解除可能切分了源所在 VMA，重新定位。
        let area_key = *self
            .areas
            .floor(&old_start_vpn)
            .map(|(key, _)| key)
            .expect("remap source VMA must survive destination unmap");

        // 7. 别名：新 VMA 共享同一 backing，源保持不动。
        if old_len == 0 {
            let area = &self.areas[&area_key];
            let relative = old_start_vpn.as_usize() - area.vpn_range.start.as_usize();
            let start = VirtualAddress::from(destination_start);
            let end = VirtualAddress::from(destination_range.end);
            let alias = if let Some(shared) = &area.shared_anonymous {
                let mut view = shared.clone();
                view.page_offset += relative;
                MapArea::shared_anonymous(start, end, permission, view)
            } else {
                let file = area.shared_file.as_ref().expect("alias source is shared");
                MapArea::shared_file(
                    start,
                    end,
                    permission,
                    file.mapping.clone(),
                    alias_pages.expect("alias file range was validated"),
                )
            };
            self.push(alias, None).map_err(RemapError::Memory)?;
            return Ok(destination_address);
        }

        // 8. 切出源范围（沿用 munmap 的切分）；old_len > new_len 时把超出的尾部切给退休 Vec。
        let area = self
            .remove_area(&area_key)
            .expect("remap source VMA must remain present");
        let (left, middle, right) = area.partition_protectable(old_start_vpn, old_end_vpn);
        let (mut moved, discarded) = if old_len > new_len {
            let split = VirtualPageNumber::from_vpn(old_start_vpn.as_usize() + new_pages);
            let (_, kept, tail) = middle.partition_protectable(old_start_vpn, split);
            (kept, tail)
        } else {
            (middle, None)
        };
        let mut commit = TranslationCommit::new();
        moved.unmap(&mut self.page_table, &mut commit);
        if let Some(mut tail) = discarded {
            tail.unmap(&mut self.page_table, &mut commit);
            discarded_areas.push(tail);
        }
        moved.rebase(destination_start);
        if growing {
            moved.extend_tail(extra_pages);
        }

        // 9. 发布：left/right 留在原处，middle 出现在新地址，DONTUNMAP 在旧范围留下新的空匿名映射。
        if let Some(left) = left {
            let slot = slots.pop().expect("reserved left slot");
            self.commit_area(slot.fill(left.vpn_range.start, left));
        }
        if let Some(right) = right {
            let slot = slots.pop().expect("reserved right slot");
            self.commit_area(slot.fill(right.vpn_range.start, right));
        }
        let slot = slots.pop().expect("reserved destination slot");
        self.commit_area(slot.fill(destination_start, moved));
        if dont_unmap {
            let slot = slots.pop().expect("reserved DONTUNMAP slot");
            let fresh = MapArea::anonymous(
                VirtualAddress::from(old_start_vpn),
                VirtualAddress::from(old_end_vpn),
                permission,
            );
            self.commit_area(slot.fill(old_start_vpn, fresh));
        }
        *source_fence = Some(PreparedAreaRetirement::rehoming(discarded_areas, commit));
        self.merge_anonymous_neighbors(destination_start);
        Ok(destination_address)
    }
}
