use super::*;

/// @description TaskManager 使用的不透明 futex wait identity；memory owner 负责把用户地址
/// 归一化为 address-space、匿名共享 backing 或 shared-file offset。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FutexKey {
    /// 私有映射只在同一个 AddressSpace 内共享等待队列。
    Private {
        address_space: usize,
        address: usize,
    },
    /// 匿名共享映射以不可复用的 backing ID 和字节偏移跨进程匹配。
    SharedAnonymous { backing: u64, offset: usize },
    /// 文件共享映射以 inode identity 和文件字节偏移跨地址空间匹配。
    SharedFile { file: SharedFileId, offset: u64 },
}

impl MemorySet {
    /// @description 将 futex 用户地址归一化为稳定 wait identity，并在同一地址空间锁内
    /// 验证该 u32 当前可读。
    ///
    /// @param address 4-byte aligned futex word 地址。
    /// @param address_space private mapping 的唯一 AddressSpace identity。
    /// @param private 强制使用 address-space key；false 时共享 VMA 使用 backing/file key。
    /// @return 可读地址对应的 key；未映射、越界或共享 fault 失败返回 user fault。
    pub(crate) fn futex_key(
        &mut self,
        address: usize,
        address_space: usize,
        private: bool,
    ) -> Result<FutexKey, UserAccessError> {
        if address == 0 || address & 3 != 0 {
            return Err(UserAccessError::Fault);
        }
        self.prepare_user_read(address, core::mem::size_of::<u32>())?;
        if private {
            return Ok(FutexKey::Private {
                address_space,
                address,
            });
        }
        let vpn = VirtualAddress::from(address).floor();
        let (_, area) = self
            .areas
            .range(..=vpn)
            .next_back()
            .filter(|(_, area)| vpn < area.vpn_range.end)
            .ok_or(UserAccessError::Fault)?;
        let page_delta = vpn.as_usize() - area.vpn_range.start.as_usize();
        let page_offset = VirtualAddress::from(address).page_offset();
        if let Some(shared) = &area.shared_anonymous {
            let offset = shared
                .page_offset
                .checked_add(page_delta)
                .and_then(|page| page.checked_mul(config::PAGE_SIZE))
                .and_then(|base| base.checked_add(page_offset))
                .ok_or(UserAccessError::Overflow)?;
            return Ok(FutexKey::SharedAnonymous {
                backing: shared.backing.id,
                offset,
            });
        }
        if let Some(shared) = &area.shared_file {
            let offset = shared
                .file_offset
                .checked_add((page_delta * config::PAGE_SIZE + page_offset) as u64)
                .ok_or(UserAccessError::Overflow)?;
            return Ok(FutexKey::SharedFile {
                file: shared.mapping.id(),
                offset,
            });
        }
        Ok(FutexKey::Private {
            address_space,
            address,
        })
    }
}
