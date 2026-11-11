//! RFC 2545 §3 的出口共链路证明；只借用 NET 地址观测及真实 TCP 登记，不由 Identifier 推断。
use super::{NextHop, Peer, RoutingDomain, RoutingError};
use alloc::vec::Vec;
use core::{
    net::{IpAddr, Ipv6Addr},
    num::NonZeroU32,
};
use packet_wire::PacketError;

/// 已由 NET/传输 adapter 证明可使用的 IPv6 接口地址；不是配置意图、Linux 地址状态或 FIB。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExportAddress {
    /// 运行时接口身份；不能从持久名称或硬件地址猜测。
    pub interface: NonZeroU32,
    /// 已通过 DAD，所在接口已启用且未报告失去载波的实际地址。
    pub address: Ipv6Addr,
    /// 地址所属的子网位数，0..=128；地址本身可以有主机位。
    pub prefix_length: u8,
    /// NET 是否实际建立该地址的同前缀直连意图。
    pub on_link: bool,
}

impl RoutingDomain {
    /// 替换出口使用的合格 IPv6 地址观测；协议库不接管 NET 的传输类型或配置职责。
    /// # Parameters
    /// `addresses` 由 adapter 从同域的当前地址与接口资格投影，克隆的迭代器必须读取同一冻结输入。
    /// # Returns
    /// 地址资格或内容变化返回 true；调用者须有界重算使用 IPv6 下一跳的出口，包括先前不能发布的目标。
    /// # Errors
    /// 地址/长度非法或捕获存储不足；失败保留此前投影，不产生部分更新。
    pub fn observe_export_network(
        &mut self,
        addresses: impl Iterator<Item = ExportAddress> + Clone,
    ) -> Result<bool, RoutingError> {
        if self
            .export_addresses
            .as_ref()
            .is_some_and(|old| old.iter().copied().eq(addresses.clone()))
        {
            return Ok(false);
        }
        if addresses.clone().any(|row| {
            row.prefix_length > 128
                || row.address.is_unspecified()
                || row.address.is_multicast()
                || row.address.to_ipv4_mapped().is_some()
        }) {
            return Err(RoutingError::InvalidNetwork);
        }
        let mut next = Vec::new();
        next.try_reserve_exact(addresses.clone().count())
            .map_err(|_| RoutingError::NoMemory)?;
        next.extend(addresses);
        self.export_addresses = Some(next);
        Ok(true)
    }

    fn link_interface(&self, peer: Peer) -> Result<Option<u32>, PacketError> {
        let IpAddr::V6(address) = peer.address else {
            return Ok(None);
        };
        if address.is_unicast_link_local() {
            let interface = self
                .neighbors
                .get(&peer.id)
                .and_then(|neighbor| neighbor.source.next_hop_interface)
                .ok_or(PacketError::Unsupported)?;
            if !self
                .export_addresses
                .iter()
                .flatten()
                .any(|row| row.interface == interface && row.address.is_unicast_link_local())
            {
                return Err(PacketError::Unsupported);
            }
            return Ok(Some(interface.get()));
        }
        let mut interface = None;
        for row in self
            .export_addresses
            .iter()
            .flat_map(|addresses| addresses.iter().copied())
        {
            if row.on_link && subnet_contains(row, address) {
                if interface.is_some_and(|old| old != row.interface.get()) {
                    // 多条链路覆盖同一全局邻居时没有唯一范围证明，不能任意选一个接口的链路地址。
                    return Err(PacketError::Unsupported);
                }
                interface = Some(row.interface.get());
            }
        }
        Ok(interface)
    }

    pub(crate) fn export_next_hop(
        &self,
        target: Peer,
        address: IpAddr,
        original: Option<(Peer, NextHop)>,
        next_hop_self: bool,
    ) -> Result<NextHop, PacketError> {
        let IpAddr::V6(global) = address else {
            return Ok(NextHop {
                address,
                link_local: None,
            });
        };
        let interface = self.link_interface(target)?;
        let global = if next_hop_self && global.is_unicast_link_local() {
            let interface = interface.ok_or(PacketError::Unsupported)?;
            self.export_addresses
                .iter()
                .flatten()
                .filter(|row| !row.address.is_unicast_link_local() && !row.address.is_loopback())
                .min_by_key(|row| (row.interface.get() != interface, row.address))
                .map(|row| row.address)
                .ok_or(PacketError::Unsupported)?
        } else {
            global
        };
        let mut result = NextHop {
            address: global.into(),
            link_local: None,
        };
        let local = self
            .export_addresses
            .iter()
            .flatten()
            .any(|row| row.address == global);
        if next_hop_self && self.export_addresses.is_some() && !local {
            return Err(PacketError::Unsupported);
        }
        let Some(interface) = interface else {
            return Ok(result);
        };
        let rows = || {
            self.export_addresses.iter().flat_map(|addresses| {
                addresses
                    .iter()
                    .copied()
                    .filter(|row| row.interface.get() == interface)
            })
        };
        // 本机另一接口/回环的全局地址也标识同一节点；辅助地址仍必须属于接收邻居的实际链路。
        if !local && !rows().any(|row| row.on_link && subnet_contains(row, global)) {
            // 下一跳与接收邻居不在已证明的共同链路上；不能把上游的链路地址传播过去。
            return Ok(result);
        }
        result.link_local = if local {
            rows()
                .map(|row| row.address)
                .filter(Ipv6Addr::is_unicast_link_local)
                .min()
        } else if let Some((source, hop)) = original
            && hop.address == address
            && self.link_interface(source)? == Some(interface)
        {
            hop.link_local.filter(Ipv6Addr::is_unicast_link_local)
        } else {
            None
        };
        // 已证明需要链路地址却没有合格值时，不能用本机地址冒充第三方或静默发不完整的下一跳。
        result.link_local.ok_or(PacketError::Unsupported)?;
        Ok(result)
    }
}

fn subnet_contains(row: ExportAddress, address: Ipv6Addr) -> bool {
    let base = row.address;
    let mask = u128::MAX
        .checked_shl(128 - u32::from(row.prefix_length))
        .unwrap_or(0);
    u128::from(base) & mask == u128::from(address) & mask
}
