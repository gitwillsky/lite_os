use super::PortError;
use core::net::IpAddr;

/// 一个本地绑定覆盖的地址空间；IPv6 通配可同时覆盖两族，mapped 地址归入 IPv4。
///
/// 只描述端口地址冲突，不拥有接口选择或协议 tuple；具体地址必须由调用者先完成本地地址准入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in super::super) struct PortBinding {
    pub(super) address: Option<IpAddr>,
    // OWNER: 地址空间的位集合，bit 0/1 分别为 IPv4/IPv6；只由规范化构造器生成。
    // 缺少此区分会让 V6ONLY 的 :: 错误占用 IPv4，或让双栈 :: 漏掉 IPv4 冲突。
    pub(super) families: u8,
    pub(super) interface: Option<u32>,
}

impl PortBinding {
    /// 规范化本地地址与 V6ONLY policy，返回唯一冲突身份。
    ///
    /// # Parameters
    /// - `address`: 实际本地 IP；未指定地址表示该族通配。
    /// - `ipv6_only`: IPv6 绑定是否排除 IPv4；IPv4 输入不使用此值。
    /// # Returns
    /// 通配范围或精确地址；IPv4-mapped IPv6 与原生 IPv4 使用同一身份。
    /// # Errors
    /// V6ONLY 与 mapped 地址组合返回 `InvalidBinding`，不取得任何 membership。
    pub(in super::super) fn new(address: IpAddr, ipv6_only: bool) -> Result<Self, PortError> {
        let (address, families) = match address {
            IpAddr::V4(address) => (IpAddr::V4(address), 1),
            IpAddr::V6(address) => {
                if let Some(mapped) = address.to_ipv4_mapped() {
                    if ipv6_only {
                        return Err(PortError::InvalidBinding);
                    }
                    (IpAddr::V4(mapped), 1)
                } else {
                    (
                        IpAddr::V6(address),
                        if address.is_unspecified() && !ipv6_only {
                            3
                        } else {
                            2
                        },
                    )
                }
            }
        };
        Ok(Self {
            address: (!address.is_unspecified()).then_some(address),
            families,
            interface: None,
        })
    }

    /// 将实际逻辑接口纳入同一绑定空间；None 覆盖所有接口，非零序号由调用者在 NET 中验证。
    /// # Returns
    /// 不分配的绑定值；不同非零接口互不重叠，0 不是合法已绑定接口身份。
    pub(in super::super) fn with_interface(self, interface: Option<u32>) -> Self {
        assert_ne!(
            interface,
            Some(0),
            "bound interface requires a nonzero identity"
        );
        Self { interface, ..self }
    }

    pub(super) fn families(self) -> impl Iterator<Item = usize> {
        (0..2).filter(move |index| self.families & (1 << index) != 0)
    }

    pub(super) fn contains(self, exact: Self) -> bool {
        exact.address.is_some()
            && (self.interface.is_none() || self.interface == exact.interface)
            && self.families & exact.families == exact.families
            && self
                .address
                .is_none_or(|address| Some(address) == exact.address)
    }
}
