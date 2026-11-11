//! RIB 的静态地址族映射；递归查找、准入策略与执行投影使用同一宽度和族校验。
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use network_protocol::routes::RouteAddress;
use prefix_index::Address;
/// 索引地址到执行协议地址的静态转换；Address 仅允许 u32/u128，不能扩展出未知 wire 族。
pub trait RibAddress: Address {
    /// 对应的标准网络地址类型。
    type Ip: RouteAddress;
    /// 将规范网络数值投影为同宽地址，不截断。
    fn address(self) -> Self::Ip;
    /// 验证递归地址族并转为同宽索引键，不允许跨族截断。
    fn from_ip(address: IpAddr) -> Option<Self>;
}
impl RibAddress for u32 {
    type Ip = Ipv4Addr;
    fn address(self) -> Self::Ip {
        self.into()
    }
    fn from_ip(address: IpAddr) -> Option<Self> {
        match address {
            IpAddr::V4(address) => Some(Self::from(address)),
            _ => None,
        }
    }
}
impl RibAddress for u128 {
    type Ip = Ipv6Addr;
    fn address(self) -> Self::Ip {
        self.into()
    }
    fn from_ip(address: IpAddr) -> Option<Self> {
        match address {
            IpAddr::V6(address) => Some(Self::from(address)),
            _ => None,
        }
    }
}
