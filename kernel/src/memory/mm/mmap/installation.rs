//! fixed replacement 与新 VMA publication 共用一次 MemorySet guard，旧资源在 guard 外退休。
use super::*;

/// 已完成唯一 MM mutation 的结果；失败的 fixed 安装也必须 fence 被撤销的旧映射。
pub(crate) struct PreparedUserMapping {
    result: Result<usize, MemoryError>,
    retirement: Option<PreparedAreaRetirement>,
}
impl PreparedUserMapping {
    /// caller 已释放 MM guard；先完成旧 translation fence，再返回成功地址或安装错误。
    pub(crate) fn synchronize(self) -> Result<usize, MemoryError> {
        if let Some(retirement) = self.retirement {
            retirement.synchronize();
        }
        self.result
    }
}
impl MemorySet {
    /// 在单一 MM transaction 内完成选址、可选 replacement 与新 VMA publication。
    /// caller 在 MM guard 外保活 source，随后在 guard 外消费返回的 retirement。
    pub(crate) fn prepare_user_mapping(
        &mut self,
        request: UserMappingRequest,
        limits: MappingResourceLimits,
    ) -> Result<PreparedUserMapping, MemoryError> {
        let UserMappingRequest {
            address,
            length,
            permission,
            placement,
            source,
        } = request;
        let exact = placement != MappingPlacement::Hint;
        if !permission.contains(MapPermission::U) || exact && address == 0 {
            return Err(MemoryError::InvalidRange);
        }
        // 1. 长度不依赖 placement；零 hint 交由实际 VMA owner 选址，不能当作零 fixed 地址拒绝。
        if length == 0 {
            return Err(MemoryError::InvalidRange);
        }
        let pages = length
            .checked_add(config::PAGE_SIZE - 1)
            .ok_or(MemoryError::InvalidRange)?
            / config::PAGE_SIZE;
        // 2. 只有 exact 请求拥有预先确定的完整范围；hint 在各 backing 安装时按页向下取整。
        let exact_range = if exact {
            Some(Self::checked_page_range(address, length)?)
        } else {
            None
        };
        match &source {
            UserMappingSource::PrivateFile(file) | UserMappingSource::SharedFile(file) => {
                if file.pages.count() != pages as u64 {
                    return Err(MemoryError::InvalidRange);
                }
            }
            UserMappingSource::Device(device) => {
                if permission.contains(MapPermission::X) {
                    return Err(MemoryError::PermissionDenied);
                }
                if device
                    .page_offset
                    .checked_add(pages)
                    .is_none_or(|end| end > device.backing.pages())
                {
                    return Err(MemoryError::InvalidRange);
                }
            }
            UserMappingSource::SystemV(shared) => {
                if shared.backing.page_count != pages
                    || permission.contains(MapPermission::W) && !shared.may_write
                {
                    return Err(MemoryError::PermissionDenied);
                }
            }
            UserMappingSource::PrivateAnonymous | UserMappingSource::SharedAnonymous => {}
        }
        let added = pages as u64 * config::PAGE_SIZE as u64;
        let private_writable = permission.contains(MapPermission::W)
            && matches!(
                &source,
                UserMappingSource::PrivateAnonymous | UserMappingSource::PrivateFile(_)
            );
        let (removed_virtual, removed_data) = if placement == MappingPlacement::Fixed {
            let range = exact_range.expect("fixed placement has a validated range");
            // 与 Linux __mmap_setup/may_expand_vm 一致：在旧 PTE/VMA 撤销前检查净 usage。
            // 从 ordered floor 开始，只遍历实际相交节点；不扫描不相关 VMA。
            let first = self
                .areas
                .floor(&range.start)
                .map_or(range.start, |(key, _)| *key);
            self.areas
                .iter_from(&first)
                .take_while(|(key, _)| **key < range.end)
                .filter(|(_, area)| range.start < area.vpn_range.end)
                .fold((0u64, 0u64), |(virtual_bytes, data_bytes), (_, area)| {
                    let overlap = (range.end.min(area.vpn_range.end).as_usize()
                        - range.start.max(area.vpn_range.start).as_usize())
                        as u64
                        * config::PAGE_SIZE as u64;
                    let contribution = area.index_contribution();
                    (
                        virtual_bytes
                            + if contribution.virtual_bytes != 0 {
                                overlap
                            } else {
                                0
                            },
                        data_bytes
                            + if contribution.data_bytes != 0 {
                                overlap
                            } else {
                                0
                            },
                    )
                })
        } else {
            (0, 0)
        };
        if self
            .virtual_bytes()
            .checked_sub(removed_virtual)
            .and_then(|current| current.checked_add(added))
            .is_none_or(|total| total > limits.address_space)
            || private_writable
                && self
                    .data_bytes()
                    .checked_sub(removed_data)
                    .and_then(|current| current.checked_add(added))
                    .is_none_or(|total| total > limits.data)
        {
            return Err(MemoryError::OutOfMemory);
        }
        let retirement = if placement == MappingPlacement::Fixed {
            Some(self.prepare_user_unmap(address, length)?)
        } else {
            None
        };
        // 首个旧 VMA 摘除后不能用 ? 提前抛弃 retirement；安装结果始终与 fence token 一同返回。
        let result = match source {
            UserMappingSource::PrivateAnonymous => self.map_anonymous(
                address,
                length,
                permission,
                exact,
                limits.address_space,
                limits.data,
            ),
            UserMappingSource::SharedAnonymous => {
                self.map_shared_anonymous(address, length, permission, exact, limits.address_space)
            }
            UserMappingSource::PrivateFile(file) => {
                self.map_private_file(address, permission, exact, file, limits)
            }
            UserMappingSource::SharedFile(file) => {
                self.map_shared_file(address, permission, exact, file, limits.address_space)
            }
            UserMappingSource::Device(device) => self.map_device(
                address,
                length,
                permission,
                exact,
                device,
                limits.address_space,
            ),
            UserMappingSource::SystemV(shared) => self.map_system_v_shared_memory(
                address,
                length,
                permission,
                shared,
                limits.address_space,
            ),
        };
        Ok(PreparedUserMapping { result, retirement })
    }
}
