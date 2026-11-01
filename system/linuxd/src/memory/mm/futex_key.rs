use super::*;

/// 两地址 futex access 的语义错误；保留 source fault 与随后 target alignment 的原生先后顺序。
#[derive(Debug)]
pub(crate) enum FutexAccessError {
    Unaligned,
    Memory(UserAccessError),
}
impl From<UserAccessError> for FutexAccessError {
    fn from(error: UserAccessError) -> Self {
        Self::Memory(error)
    }
}

/// ProcessTable 使用的不透明 futex wait identity；memory owner 负责把用户地址
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
    /// Device VMA 以不复用 backing identity 与 byte offset 跨 fork 匹配。
    SharedDevice { backing: u64, offset: usize },
}

impl FutexKey {
    /// private key 的 native access_ok；无需读取 VMA 或 fault-in，也允许合法范围内的零地址。
    pub(crate) fn private(address: usize, address_space: usize) -> Result<Self, UserAccessError> {
        if address & 3 != 0
            || address
                .checked_add(4)
                .is_none_or(|end| end > config::USER_ADDRESS_END)
        {
            return Err(UserAccessError::Fault);
        }
        Ok(Self::Private {
            address_space,
            address,
        })
    }
}

impl MemorySet {
    /// 将 futex 用户地址归一化为稳定 wait identity，并在同一地址空间锁内
    /// 验证该 u32 当前可读。
    ///
    /// # Parameters
    ///
    /// - `address`: 4-byte aligned futex word 地址。
    /// - `address_space`: private mapping 的唯一 AddressSpace identity。
    /// - `private`: 强制使用 address-space key；false 时共享 VMA 使用 backing/file key。
    ///
    /// # Returns
    ///
    /// 可读地址对应的 key；未映射、越界或共享 fault 失败返回 user fault。
    pub(crate) fn futex_key(
        &mut self,
        address: usize,
        address_space: usize,
        private: bool,
        limits: UserFaultLimits,
    ) -> Result<FutexKey, UserAccessError> {
        if private {
            // native private key 只需 access_ok，不 fault-in VMA；WAIT/比较/写操作另行验证实际 word。
            return FutexKey::private(address, address_space);
        }
        if address & 3 != 0 {
            return Err(UserAccessError::Fault);
        }
        self.prepare_user_read(address, core::mem::size_of::<u32>(), limits)?;
        let vpn = VirtualAddress::from(address).floor();
        let (_, area) = self
            .areas
            .floor(&vpn)
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
                .byte_within(area.vpn_range.start, vpn, page_offset)
                .ok_or(UserAccessError::Overflow)?;
            return Ok(FutexKey::SharedFile {
                file: shared.mapping.id(),
                offset,
            });
        }
        if let Some(device) = &area.device {
            let offset = (device.page_offset as usize)
                .checked_add(page_delta)
                .and_then(|page| page.checked_mul(config::PAGE_SIZE))
                .and_then(|base| base.checked_add(page_offset))
                .ok_or(UserAccessError::Overflow)?;
            return Ok(FutexKey::SharedDevice {
                backing: device.identity,
                offset,
            });
        }
        Ok(FutexKey::Private {
            address_space,
            address,
        })
    }
}
