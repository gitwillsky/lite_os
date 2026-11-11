//! Socket 的双栈 ASM 资格与接口 IGMP/MLD 引用；所有变更/关闭都在原 NET guard 内提交。
use super::{InetEndpoint, InetSocket, NetworkInterface, SocketError};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub(in crate::inet) mod ipv4;
pub(in crate::inet) mod listener;

/// 定时到期或等待实际发送容量；Writable 不产生立即重投的 timer，设备活动再驱动它。
#[derive(Clone, Copy)]
pub(in crate::inet) enum SendTime {
    At(u64),
    Writable,
}
impl SendTime {
    pub(in crate::inet) fn ready(self, now: u64) -> bool {
        matches!(self, Self::Writable) || self.deadline().is_some_and(|at| at <= now)
    }
    pub(in crate::inet) fn deadline(self) -> Option<u64> {
        match self {
            Self::At(at) => Some(at),
            Self::Writable => None,
        }
    }
}

pub(in crate::inet) fn random_delay(maximum: u64) -> u64 {
    if maximum == 0 {
        return 0;
    }
    // 仅控制报文计时取随机数；拒绝模偏差，不建立全局 PRNG 或逐包随机状态。
    let minimum = maximum.wrapping_neg() % maximum;
    loop {
        let mut bytes = [0; 8];
        native_runtime::system::cprng_draw(&mut bytes);
        let value = u64::from_le_bytes(bytes);
        if value >= minimum {
            return 1 + value % maximum;
        }
    }
}

/// TTL/Hop Limit 0 与 IPv6 接口内组播只回送本机；lo 保留自身的单次投递。
pub(super) fn on_link(destination: IpAddr, hops: u8, interface: u32) -> bool {
    interface == 1
        || !destination.is_multicast()
        || hops != 0 && !matches!(destination, IpAddr::V6(group) if group.octets()[1] & 15 <= 1)
}

/// 通过原有 lo 队列投递一次完整双栈组播；入接口保留实际发送口，仍经成员和协议接收检查。
/// 本机队列拥塞只丢弃这份回送；调用方消费回送义务后再尝试 NIC，链路背压不重复投递。
pub(super) fn loopback(
    interfaces: &[NetworkInterface],
    interface: u32,
    destination: IpAddr,
    length: usize,
    timestamp: smoltcp::time::Instant,
    emit: impl FnOnce(&mut [u8]),
) {
    use smoltcp::phy::TxToken;
    if interface == 1 || !destination.is_multicast() {
        return;
    }
    if !interfaces
        .iter()
        .find(|entry| entry.index == interface)
        .is_some_and(|entry| {
            entry.state.up
                && match destination {
                    IpAddr::V4(group) => entry.igmp.accepts(group),
                    IpAddr::V6(group) => entry
                        .ipv6
                        .as_ref()
                        .is_some_and(|owner| owner.accepts_destination(group)),
                }
        })
    {
        return;
    }
    let Some(entry) = interfaces
        .iter()
        .find(|entry| entry.index == 1 && entry.state.up)
    else {
        return;
    };
    if length > entry.device.ip_mtu() {
        entry.device.record_transmit_drop();
        return;
    }
    let Some(mut token) = entry.device.transmit_shared(timestamp) else {
        entry.device.record_transmit_drop();
        return;
    };
    let mut meta = smoltcp::phy::PacketMeta::default();
    meta.interface_index = Some(interface);
    token.set_meta(meta);
    let (_, failure) = token.consume_committed(
        length,
        if destination.is_ipv4() {
            0x0800
        } else {
            0x86dd
        },
        emit,
    );
    if failure.is_some() {
        entry.device.record_transmit_drop();
    }
}

/// Socket 的真实组/接口引用；address 保存 IPv4 请求选择器，不镜像当前设备地址。
struct Membership {
    group: IpAddr,
    interface: u32,
    address: Ipv4Addr,
}
impl Membership {
    fn release(self, interfaces: &mut [NetworkInterface], now: u64) {
        if let Some(entry) = interfaces
            .iter_mut()
            .find(|entry| entry.index == self.interface)
        {
            match self.group {
                IpAddr::V4(group) => entry.igmp.drop_group(group, now),
                IpAddr::V6(group) => {
                    if let Some(owner) = &mut entry.ipv6 {
                        owner.drop_group(group, now);
                    }
                }
            }
        }
    }
}
/// 唯一 socket 成员表，按 (组、实际 ifindex) 排序；未加入组时不分配。
#[derive(Default)]
pub(super) struct Memberships(Vec<Membership>);
impl Memberships {
    pub(super) fn accepts(&self, destination: IpAddr, interface: u32, all: bool) -> bool {
        !destination.is_multicast()
            || all
            || self
                .0
                .binary_search_by_key(&(destination, interface), |member| {
                    (member.group, member.interface)
                })
                .is_ok()
    }
    /// 关闭消费全部接口引用；热拔出的 ifindex 不复用，不能把旧成员转给新设备。
    pub(super) fn close(self, interfaces: &mut [NetworkInterface], now: u64) {
        for member in self.0 {
            member.release(interfaces, now);
        }
    }
}
impl InetSocket {
    /// 加入或退出 IPv6 ASM 组，不改变绑定地址、端口或发送策略。
    /// # Parameters
    /// `group` 为组播地址；`interface` 为实际 ifindex，0 加入时选择接口、退出时匹配任一同组成员。
    /// `join` 为 true 加入、false 退出。
    /// # Returns
    /// Socket 资格与接口引用在同一 guard 内完整发布后返回 unit。
    /// # Errors
    /// 非法组、类型不支持、重复加入、成员/设备不存在或 OOM；失败保留成员与协议义务。
    pub(crate) fn ipv6_membership(
        &self,
        group: Ipv6Addr,
        interface: u32,
        join: bool,
    ) -> Result<(), SocketError> {
        self.membership(group.into(), interface, Ipv4Addr::UNSPECIFIED, join)
    }
    /// 修改 IPv4 ASM 成员；接口序号优先于接口地址，均未指定时按实际 IPv4 路由选择。
    /// # Parameters
    /// `group` 为组播地址；`interface` 为非零 NET ifindex 或 0；`address` 为地址选择器或 0.0.0.0。
    /// `join` 为 true 加入、false 退出。
    /// # Returns
    /// Socket 与 IGMP 引用完整提交后返回 unit。
    /// # Errors
    /// 非法组、TCP、重复/不存在成员、未知接口/地址、无默认路径或 OOM，不留下半个引用。
    pub(crate) fn ipv4_membership(
        &self,
        group: Ipv4Addr,
        interface: u32,
        address: Ipv4Addr,
        join: bool,
    ) -> Result<(), SocketError> {
        self.membership(group.into(), interface, address, join)
    }
    fn membership(
        &self,
        group: IpAddr,
        interface: u32,
        address: Ipv4Addr,
        join: bool,
    ) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        if !group.is_multicast() {
            return Err(SocketError::Invalid);
        }
        let mut network = self.stack()?.lock()?;
        let super::NetworkStack {
            udp_endpoints,
            raw_endpoints,
            interfaces,
            routing,
            ..
        } = &mut *network;
        let members = match self.endpoint {
            InetEndpoint::Udp(id) => {
                let state = udp_endpoints
                    .get_mut(&id)
                    .ok_or(SocketError::NotConnected)?;
                if group.is_ipv6() && !state.is_ipv6() {
                    return Err(SocketError::OperationNotSupported);
                }
                &mut state.memberships
            }
            InetEndpoint::Raw(id) => {
                let state = raw_endpoints
                    .get_mut(&id)
                    .ok_or(SocketError::NotConnected)?;
                if group.is_ipv6() && !state.is_ipv6() {
                    return Err(SocketError::OperationNotSupported);
                }
                &mut state.memberships
            }
            InetEndpoint::Tcp(_) => return Err(SocketError::OperationNotSupported),
        };
        let selected = if interface != 0 {
            Some(interface)
        } else if let IpAddr::V4(group) = group {
            if !address.is_unspecified() {
                interfaces
                    .iter()
                    .find(|entry| entry.state.address == Some(address))
                    .map(|entry| entry.index)
            } else {
                routing
                    .select_facts(
                        Ipv4Addr::UNSPECIFIED,
                        group,
                        None,
                        interfaces.iter().map(super::route::InterfaceFacts::capture),
                    )
                    .ok()
                    .map(|route| route.logical_interface_index)
            }
        } else {
            None
        };
        if group.is_ipv4()
            && (join
                && selected
                    .is_none_or(|index| !interfaces.iter().any(|entry| entry.index == index))
                || !join && interface == 0 && address.is_unspecified() && selected.is_none())
        {
            return Err(SocketError::NoDevice);
        }
        let previous = members.0.iter().position(|member| {
            member.group == group
                && match selected {
                    Some(index) => member.interface == index,
                    None => {
                        !group.is_ipv4() || address.is_unspecified() || member.address == address
                    }
                }
        });
        let now = super::now().total_millis().max(0) as u64;
        if !join {
            let position = previous.ok_or(SocketError::AddressNotAvailable)?;
            let member = members.0.remove(position);
            if members.0.is_empty() {
                members.0 = Vec::new();
            }
            member.release(interfaces, now);
            return Ok(());
        }
        if previous.is_some() {
            return Err(SocketError::AddressInUse);
        }
        let selected = selected.or_else(|| {
            super::route::default_multicast_interface(
                interfaces
                    .iter()
                    .map(super::route::Ipv6InterfaceFacts::capture),
            )
        });
        let entry = interfaces
            .iter_mut()
            .find(|entry| Some(entry.index) == selected)
            .ok_or(SocketError::NoDevice)?;
        members
            .0
            .try_reserve(1)
            .map_err(|_| SocketError::NoMemory)?;
        // 1. Socket 容量先准备。2. 接口引用准备/提交。3. 不再失败地发布成员。
        let joined = match group {
            IpAddr::V4(group) => entry.igmp.add(group, now),
            IpAddr::V6(group) => entry
                .ipv6
                .as_mut()
                .ok_or(SocketError::NoDevice)
                .and_then(|owner| owner.add_group(group, now)),
        };
        if let Err(error) = joined {
            if members.0.is_empty() {
                members.0 = Vec::new();
            }
            return Err(error);
        }
        let position = members
            .0
            .binary_search_by_key(&(group, entry.index), |member| {
                (member.group, member.interface)
            })
            .unwrap_err();
        members.0.insert(
            position,
            Membership {
                group,
                interface: entry.index,
                address,
            },
        );
        Ok(())
    }
}
