//! Internet 选项的服务协议编码；Linux 编号和应用授权不属于这里。
use super::*;

/// Current endpoint policy bounds RFC 2385 secrets to the interoperable TCP_MD5SIG key size.
pub const TCP_MD5_MAX_KEY_BYTES: usize = 80;
/// Peer prefix and optional logical input/output interface for a TCP signature key.
/// Interface scoping is explicit service policy; no Linux sockaddr or capability lives here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpMd5Selector {
    pub address: core::net::IpAddr,
    pub prefix_length: u8,
    pub interface_index: Option<u32>,
}
impl TcpMd5Selector {
    /// Select one exact peer, optionally within its current logical interface.
    pub fn host(address: core::net::IpAddr, interface_index: Option<u32>) -> Self {
        Self {
            address,
            prefix_length: if address.is_ipv4() { 32 } else { 128 },
            interface_index,
        }
    }
    /// Validate family width and a real optional interface identity; mapped aliases are not wire peers.
    pub fn valid(self) -> bool {
        self.prefix_length <= if self.address.is_ipv4() { 32 } else { 128 }
            && self.interface_index != Some(0)
            && !matches!(self.address, core::net::IpAddr::V6(ip) if ip.to_ipv4_mapped().is_some())
    }
    /// Match this prefix in the selected interface; the caller owns which interface is authoritative.
    pub fn matches(self, address: core::net::IpAddr, interface: u32) -> bool {
        if !self.valid() || self.interface_index.is_some_and(|index| index != interface) {
            return false;
        }
        match (self.address, address) {
            (core::net::IpAddr::V4(prefix), core::net::IpAddr::V4(address)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix_length))
                    .unwrap_or(0);
                u32::from(prefix) & mask == u32::from(address) & mask
            }
            (core::net::IpAddr::V6(prefix), core::net::IpAddr::V6(address)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix_length))
                    .unwrap_or(0);
                u128::from(prefix) & mask == u128::from(address) & mask
            }
            _ => false,
        }
    }
}

pub(super) fn dispatch_md5(
    service: &dyn InetSocketService,
    request: &mut Reader<'_>,
    reply: &mut Writer,
) -> Result<(), Status> {
    if request.u32()? != 1 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let endpoint = get_inet(request)?;
    let selector = TcpMd5Selector {
        address: endpoint.ip(),
        prefix_length: request.u8()?,
        interface_index: core::num::NonZeroU32::new(request.u32()?).map(core::num::NonZeroU32::get),
    };
    let key = request.bytes()?;
    request.finish()?;
    if !selector.valid()
        || key.len() > TCP_MD5_MAX_KEY_BYTES
        || endpoint != InetAddress::new(endpoint.ip(), 0)
    {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    reply.try_reserve(1)?;
    put_result(reply, service.set_md5_key(selector, key));
    Ok(())
}

/// INET socket 的选项写入。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InetOptionChange {
    HeaderIncluded(bool),
    PacketInfo(bool),
    ReuseAddress(bool),
    Broadcast(bool),
    /// 为本机输出设置完整的 32 位路由标记。
    Mark(u32),
    /// `None` 恢复默认 TTL。
    HopLimit(Option<u8>),
    NoDelay(bool),
    /// keepalive 与 TCP 定时器类选项（`KeepAlive`…`Linger2`）。
    Timer(InternetOption, i32),
    MaxSegment(i32),
    Ipv6Only(bool),
    Ipv6PacketInfo(bool),
    /// IPv6 单播跳数；None 恢复默认，0 是有效 hop limit。
    Ipv6HopLimit(Option<u8>),
    /// 显式 IPv6 发送 MTU 上限；0 恢复路径发现，非零值必须至少 1280。
    Ipv6Mtu(i32),
    /// 原始 IPv6 upper-layer 校验和字段偏移，负数禁用。
    Ipv6Checksum(i32),
    /// 是否接收本接口上其他 socket 已加入的 IPv6 组播；默认开启。
    Ipv6MulticastAll(bool),
    /// IPv6 组播默认发送接口；0 恢复网络域选择。
    Ipv6MulticastInterface(u32),
    /// IPv6 组播跳数；None 恢复默认的 1。
    Ipv6MulticastHopLimit(Option<u8>),
    /// 是否将 IPv6 组播回送给本机成员。
    Ipv6MulticastLoop(bool),
    /// 是否接收接口上其他 socket 已加入的 IPv4 组播；默认开启。
    Ipv4MulticastAll(bool),
    /// IPv4 组播发送接口和源地址选择意图；接口优先，均为零时恢复选路。
    Ipv4MulticastInterface {
        /// 实际 NET ifindex，0 表示按地址选择。
        interface: u32,
        /// 地址选择意图，0.0.0.0 表示自动选源；不镜像设备当前地址。
        address: core::net::Ipv4Addr,
    },
    /// IPv4 组播 TTL；None 恢复默认 1。
    Ipv4MulticastHopLimit(Option<u8>),
    /// 是否将 IPv4 组播回送给本机成员。
    Ipv4MulticastLoop(bool),
    /// IPv4 路径 MTU 发现模式；0..5，其他值拒绝。
    Ipv4MtuDiscovery(u8),
    Ipv6DontFragment(bool),
    ReceiveError {
        ipv6: bool,
        enabled: bool,
    },
    /// IPv4 ASM 加退组；接口序号优先，0 时按地址或实际路由选择。
    Ipv4Membership {
        /// 组播地址。
        group: core::net::Ipv4Addr,
        /// 实际 NET ifindex；0 表示未指定。
        interface: u32,
        /// 接口地址选择器；0.0.0.0 表示未指定。
        address: core::net::Ipv4Addr,
        /// true 加入，false 退出。
        join: bool,
    },
    /// 修改本 socket 的 IPv6 ASM 成员资格，不改变发送接口。
    Ipv6Membership {
        /// 要加入或退出的组播地址。
        group: core::net::Ipv6Addr,
        /// 实际 NET ifindex；0 由接收 owner 选择。
        interface: u32,
        /// true 加入，false 退出。
        join: bool,
    },
}

fn hop_limit(value: i32) -> Result<Option<u8>, Status> {
    match value {
        -1 => Ok(None),
        0..=255 => Ok(Some(value as u8)),
        _ => Err(native_abi::ERR_INVALID_ARGS),
    }
}

pub(super) fn get_option_change(request: &mut Reader<'_>) -> Result<InetOptionChange, Status> {
    let kind = request.u8()?;
    let value = request.i32()?;
    let option = request.u8()?;
    Ok(match kind {
        0 => InetOptionChange::HeaderIncluded(value != 0),
        1 => InetOptionChange::PacketInfo(value != 0),
        2 => InetOptionChange::ReuseAddress(value != 0),
        3 => InetOptionChange::Broadcast(value != 0),
        4 => InetOptionChange::HopLimit(hop_limit(value)?),
        5 => InetOptionChange::NoDelay(value != 0),
        6 => InetOptionChange::Timer(
            InternetOption::from_code(option).ok_or(native_abi::ERR_INVALID_ARGS)?,
            value,
        ),
        7 => InetOptionChange::MaxSegment(value),
        8 => InetOptionChange::Ipv6Only(value != 0),
        9 => InetOptionChange::Ipv6HopLimit(hop_limit(value)?),
        10 => InetOptionChange::Ipv6PacketInfo(value != 0),
        11 => InetOptionChange::Ipv6Mtu(value),
        12 => InetOptionChange::Ipv6Checksum(value),
        13 => InetOptionChange::Ipv6MulticastAll(value != 0),
        14 => {
            if !matches!(value, 0 | 1) || option != 0 || request.u32()? != 1 {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            let group = core::net::Ipv6Addr::from(
                <[u8; 16]>::try_from(request.bytes()?).map_err(|_| native_abi::ERR_INVALID_ARGS)?,
            );
            InetOptionChange::Ipv6Membership {
                group,
                interface: request.u32()?,
                join: value != 0,
            }
        }
        15 => InetOptionChange::Ipv6MulticastInterface(value as u32),
        16 => InetOptionChange::Ipv6MulticastHopLimit(hop_limit(value)?),
        17 if matches!(value, 0 | 1) => InetOptionChange::Ipv6MulticastLoop(value != 0),
        18 if matches!(value, 0 | 1) => InetOptionChange::Ipv4MulticastAll(value != 0),
        19 => {
            if !matches!(value, 0 | 1) || option != 0 || request.u32()? != 1 {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            InetOptionChange::Ipv4Membership {
                group: get_ipv4(request)?,
                interface: request.u32()?,
                address: get_ipv4(request)?,
                join: value != 0,
            }
        }
        20 if value == 0 && option == 0 => {
            if request.u32()? != 1 {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            InetOptionChange::Ipv4MulticastInterface {
                interface: request.u32()?,
                address: get_ipv4(request)?,
            }
        }
        21 => InetOptionChange::Ipv4MulticastHopLimit(hop_limit(value)?),
        22 if matches!(value, 0 | 1) => InetOptionChange::Ipv4MulticastLoop(value != 0),
        23 if (0..=5).contains(&value) => InetOptionChange::Ipv4MtuDiscovery(value as u8),
        24 if matches!(value, 0 | 1) && option == 0 => {
            InetOptionChange::Ipv6DontFragment(value != 0)
        }
        25 if matches!(value, 0 | 1) && option <= 1 => InetOptionChange::ReceiveError {
            ipv6: option != 0,
            enabled: value != 0,
        },
        26 if option == 0 => InetOptionChange::Mark(value as u32),
        _ => return Err(native_abi::ERR_INVALID_ARGS),
    })
}

pub(super) fn put_option_change(writer: &mut Writer, change: InetOptionChange) {
    let (kind, value, option) = match change {
        InetOptionChange::HeaderIncluded(enabled) => (0, i32::from(enabled), 0),
        InetOptionChange::PacketInfo(enabled) => (1, i32::from(enabled), 0),
        InetOptionChange::ReuseAddress(enabled) => (2, i32::from(enabled), 0),
        InetOptionChange::Mark(mark) => (26, mark as i32, 0),
        InetOptionChange::Broadcast(enabled) => (3, i32::from(enabled), 0),
        InetOptionChange::HopLimit(value) => (4, value.map_or(-1, i32::from), 0),
        InetOptionChange::NoDelay(enabled) => (5, i32::from(enabled), 0),
        InetOptionChange::Timer(option, value) => (6, value, option.code()),
        InetOptionChange::MaxSegment(value) => (7, value, 0),
        InetOptionChange::Ipv6Only(value) => (8, i32::from(value), 0),
        InetOptionChange::Ipv6HopLimit(value) => (9, value.map_or(-1, i32::from), 0),
        InetOptionChange::Ipv6PacketInfo(value) => (10, i32::from(value), 0),
        InetOptionChange::Ipv6Mtu(value) => (11, value, 0),
        InetOptionChange::Ipv6Checksum(value) => (12, value, 0),
        InetOptionChange::Ipv6MulticastAll(value) => (13, i32::from(value), 0),
        InetOptionChange::Ipv6MulticastInterface(value) => (15, value as i32, 0),
        InetOptionChange::Ipv6MulticastHopLimit(value) => (16, value.map_or(-1, i32::from), 0),
        InetOptionChange::Ipv6MulticastLoop(value) => (17, i32::from(value), 0),
        InetOptionChange::Ipv4MulticastAll(value) => (18, i32::from(value), 0),
        InetOptionChange::Ipv4MtuDiscovery(value) => (23, i32::from(value), 0),
        InetOptionChange::Ipv6DontFragment(value) => (24, i32::from(value), 0),
        InetOptionChange::ReceiveError { ipv6, enabled } => {
            (25, i32::from(enabled), u8::from(ipv6))
        }
        InetOptionChange::Ipv4MulticastInterface { interface, address } => {
            writer.u8(20).i32(0).u8(0).u32(1).u32(interface);
            put_ipv4(writer, address);
            return;
        }
        InetOptionChange::Ipv4MulticastHopLimit(value) => (21, value.map_or(-1, i32::from), 0),
        InetOptionChange::Ipv4MulticastLoop(value) => (22, i32::from(value), 0),
        InetOptionChange::Ipv4Membership {
            group,
            interface,
            address,
            join,
        } => {
            writer.u8(19).i32(i32::from(join)).u8(0).u32(1);
            put_ipv4(writer, group);
            writer.u32(interface);
            put_ipv4(writer, address);
            return;
        }
        InetOptionChange::Ipv6Membership {
            group,
            interface,
            join,
        } => {
            writer
                .u8(14)
                .i32(i32::from(join))
                .u8(0)
                .u32(1)
                .bytes(&group.octets())
                .u32(interface);
            return;
        }
    };
    writer.u8(kind).i32(value).u8(option);
}

/// ICMP 类型过滤器的原子位更新；mask 外的类型保留旧策略，blocked 位 1 表示拒绝。
/// 两组逻辑 u32 均按服务协议编码，不包含 Linux struct 的机器字节布局。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IcmpFilterUpdate {
    /// 需要更新的类型位；未选中的位保留其原有状态。
    pub mask: [u32; 8],
    /// mask 选中位的新值；1 阻止对应类型，0 放行。
    pub blocked: [u32; 8],
}
pub(super) fn dispatch_icmp_filter(
    service: &dyn InetSocketService,
    request: &mut Reader<'_>,
    reply: &mut Writer,
) -> Result<(), Status> {
    if request.u32()? != 1 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let update = if request.bool()? {
        let mut update = IcmpFilterUpdate::default();
        for (mask, blocked) in update.mask.iter_mut().zip(update.blocked.iter_mut()) {
            *mask = request.u32()?;
            *blocked = request.u32()?;
        }
        Some(update)
    } else {
        None
    };
    request.finish()?;
    reply.try_reserve(33)?;
    if let Some(filter) = put_result(reply, service.icmp_filter(update)) {
        for word in filter {
            reply.u32(word);
        }
    }
    Ok(())
}
