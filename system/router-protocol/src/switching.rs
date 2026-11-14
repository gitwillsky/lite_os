//! 已解析设备身份的 802.1Q 网桥配置。只描述管理决定，学习表与设备寿命归 NET。
use crate::routing::EncodeError;
use crate::{
    configuration::RouterAdvertisements,
    interface::{Ipv6Addresses, emit_advertisement, parse_advertisement},
};
use alloc::vec::Vec;
use core::net::Ipv4Addr;

const VERSION: u32 = 5;
const BATCH_HEADER: usize = 12;
const HEADER: usize = 18;
const GATEWAY: usize = 44;
const UNTAGGED: u16 = 1 << 15;
/// 单域端口上限，限制未知目的帧的扇出及配置捕获量。
pub const MAX_PORTS: usize = 256;

/// IEEE 802.1Q 可用于 VLAN 成员关系的 VID。
pub fn valid_vid(vid: u16) -> bool {
    (1..=4094).contains(&vid)
}

/// 校验 VLAN 网关能用于 Linux 接口查询的名称，包括长度和保留名约束。
pub fn valid_gateway_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() < 16
        && !matches!(name, b"." | b".." | b"lo")
        && name
            .iter()
            .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'/' | b':'))
}

/// 校验 VLAN 网关的可选 IPv4 单播地址及前缀长度。
pub fn valid_gateway_address(address: Option<Ipv4Addr>, prefix: u8) -> bool {
    prefix <= 32
        && address.map_or(prefix == 0, |ip| {
            !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast() && !ip.is_broadcast()
        })
}

/// 已校验的完整桥端口批次。空批次移除所有成员。
#[derive(Clone, Copy)]
pub struct Ports<'a> {
    bytes: &'a [u8],
    bridges: &'a [[u8; 4]],
    gateways: &'a [u8],
    ports: &'a [u8],
    projected: bool,
}

/// NET 在一次拓扑读取中解析出的接口身份、有效 MTU 和载波样本。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GatewayBinding {
    pub index: u32,
    pub mtu: u16,
    pub carrier: bool,
}

/// 已验证的 VLAN 接口，实际 ifindex 在 NET 配置租约内分配。
#[derive(Clone, Copy)]
pub struct Gateway<'a> {
    pub bridge: u16,
    pub vid: u16,
    pub name: [u8; 16],
    pub address: Option<Ipv4Addr>,
    pub prefix_length: u8,
    pub ipv6: Ipv6Addresses<'a>,
    pub binding: GatewayBinding,
    pub advertise: RouterAdvertisements,
}

fn split_gateway(bytes: &[u8], validate: bool) -> Option<(Gateway<'_>, &[u8])> {
    let (header, tail) = bytes.split_first_chunk::<GATEWAY>()?;
    let bridge = u16::from_le_bytes([header[0], header[1]]);
    let vid = u16::from_le_bytes([header[2], header[3]]);
    let name: [u8; 16] = header[4..20].try_into().ok()?;
    let address = Ipv4Addr::new(header[20], header[21], header[22], header[23]);
    let address = (!address.is_unspecified()).then_some(address);
    let prefix_length = header[24];
    let count = usize::from(u16::from_le_bytes([header[26], header[27]]));
    let binding = GatewayBinding {
        index: u32::from_le_bytes(header[28..32].try_into().ok()?),
        mtu: u16::from_le_bytes([header[32], header[33]]),
        carrier: header[34] == 1,
    };
    let (ipv6, tail) = tail.split_at_checked(count * Ipv6Addresses::ROW_BYTES)?;
    let dns_count = usize::try_from(u32::from_le_bytes(header[40..44].try_into().ok()?)).ok()?;
    let (dns_rows, tail) = tail.split_at_checked(dns_count.checked_mul(16)?)?;
    let advertise = parse_advertisement(&header[36..40], dns_rows).ok()?;
    let ipv6 = if validate {
        let end = name.iter().position(|&byte| byte == 0)?;
        if !valid_gateway_name(&name[..end])
            || name[end..].iter().any(|&byte| byte != 0)
            || !valid_vid(vid)
            || !valid_gateway_address(address, prefix_length)
            || header[25] != 0
            || header[34] > 1
            || header[35] != 0
            || if binding.index == 0 {
                binding != GatewayBinding::default()
            } else {
                !(2..=i32::MAX as u32).contains(&binding.index)
                    || !(68..=1500).contains(&binding.mtu)
            }
        {
            return None;
        }
        Ipv6Addresses::parse(ipv6).ok()?
    } else {
        Ipv6Addresses::validated(ipv6)
    };
    if validate && !advertise.valid_for(ipv6.iter()) {
        return None;
    }
    Some((
        Gateway {
            bridge,
            vid,
            name,
            address,
            prefix_length,
            ipv6,
            binding,
            advertise,
        },
        tail,
    ))
}

/// Settings shared by all ports of one spanning tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bridge {
    pub priority: u16,
    pub rstp: bool,
}

/// 单个实际 Ethernet 端口及其 VLAN 成员关系。
#[derive(Clone, Copy)]
pub struct Port<'a> {
    pub index: u32,
    pub bridge: u16,
    pub pvid: u16,
    pub isolated: bool,
    pub edge: bool,
    pub point_to_point: bool,
    pub number: u16,
    pub path_cost: u32,
    vlans: &'a [[u8; 2]],
}

impl<'a> Port<'a> {
    /// 按 VID 顺序返回成员，布尔值为出站去标签。
    pub fn vlans(self) -> impl ExactSizeIterator<Item = (u16, bool)> + 'a {
        self.vlans.iter().map(|row| {
            let entry = u16::from_le_bytes(*row);
            (entry & !UNTAGGED, entry & UNTAGGED != 0)
        })
    }
}

fn split_port(bytes: &[u8]) -> Option<(Port<'_>, &[u8])> {
    let (header, tail) = bytes.split_at_checked(HEADER)?;
    let index = u32::from_le_bytes(header[..4].try_into().ok()?);
    let bridge = u16::from_le_bytes(header[4..6].try_into().ok()?);
    let pvid = u16::from_le_bytes(header[6..8].try_into().ok()?);
    let count = u16::from_le_bytes(header[8..10].try_into().ok()?);
    let flags = header[10];
    let number = u16::from_le_bytes(header[12..14].try_into().ok()?);
    let path_cost = u32::from_le_bytes(header[14..18].try_into().ok()?);
    if index < 2
        || bridge == 0
        || flags & !7 != 0
        || header[11] != 0
        || !(1..=MAX_PORTS).contains(&usize::from(number))
        || !(1..=200_000_000).contains(&path_cost)
    {
        return None;
    }
    let (vlans, tail) = tail.split_at_checked(usize::from(count) * 2)?;
    Some((
        Port {
            index,
            bridge,
            pvid,
            isolated: flags & 1 != 0,
            edge: flags & 2 != 0,
            point_to_point: flags & 4 != 0,
            number,
            path_cost,
            vlans: vlans.as_chunks::<2>().0,
        },
        tail,
    ))
}

impl<'a> Ports<'a> {
    /// 验证版本、端口与 VID 的唯一顺序、PVID 成员关系和完整字节边界。
    /// # Errors
    /// 格式无效、端口数超限或 VLAN 成员冲突。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, EncodeError> {
        let (header, tail) = bytes
            .split_first_chunk::<BATCH_HEADER>()
            .ok_or(EncodeError::Invalid)?;
        let count = u16::from_le_bytes([header[4], header[5]]) as usize;
        let gateway_count = usize::from(u16::from_le_bytes([header[6], header[7]]));
        if header[..4] != VERSION.to_le_bytes()
            || count > MAX_PORTS
            || gateway_count > MAX_PORTS
            || header[8] > 1
            || header[9..] != [0; 3]
        {
            return Err(EncodeError::Invalid);
        }
        let (bridges, tail) = tail
            .split_at_checked(count * 4)
            .ok_or(EncodeError::Invalid)?;
        let bridges = bridges.as_chunks::<4>().0;
        if bridges
            .iter()
            .any(|row| row[0] != 0 || row[1] & 0xf != 0 || row[2] > 1 || row[3] != 0)
        {
            return Err(EncodeError::Invalid);
        }
        let mut ports = tail;
        let mut last = (0, 0);
        let mut names = [[0; 16]; MAX_PORTS];
        let mut indices = [0u32; MAX_PORTS];
        let projected = header[8] == 1;
        for names_at in 0..gateway_count {
            let (gateway, rest) = split_gateway(ports, true).ok_or(EncodeError::Invalid)?;
            let key = (gateway.bridge, gateway.vid);
            if gateway.bridge == 0
                || usize::from(gateway.bridge) > count
                || key <= last
                || names[..names_at].contains(&gateway.name)
                || gateway.binding.index != 0
                    && (!projected || indices[..names_at].contains(&gateway.binding.index))
            {
                return Err(EncodeError::Invalid);
            }
            names[names_at] = gateway.name;
            indices[names_at] = gateway.binding.index;
            last = key;
            ports = rest;
        }
        let result = Self {
            bytes,
            bridges,
            gateways: &tail[..tail.len() - ports.len()],
            ports,
            projected,
        };
        let mut tail = ports;
        let mut previous = 0;
        let mut assigned = [false; MAX_PORTS];
        for _ in 0..MAX_PORTS {
            if tail.is_empty() {
                return Ok(result);
            }
            let (port, next) = split_port(tail).ok_or(EncodeError::Invalid)?;
            let mut last_vid = 0;
            let mut pvid_present = port.pvid == 0;
            for (vid, _) in port.vlans() {
                if !valid_vid(vid) || vid <= last_vid {
                    return Err(EncodeError::Invalid);
                }
                pvid_present |= vid == port.pvid;
                last_vid = vid;
            }
            if port.index <= previous
                || last_vid == 0
                || !pvid_present
                || usize::from(port.bridge) > count
                || core::mem::replace(&mut assigned[usize::from(port.number) - 1], true)
            {
                return Err(EncodeError::Invalid);
            }
            previous = port.index;
            tail = next;
        }
        tail.is_empty()
            .then_some(result)
            .ok_or(EncodeError::Invalid)
    }

    /// 是否已由 NET 投影，空集合无需投影。
    pub fn projected(self) -> bool {
        self.projected || self.gateways.is_empty()
    }

    /// 将 NET 的解析结果写入同一批次，顺序与 gateways 一致。原正文和运行时状态均不变。
    /// # Errors
    /// 行数、解析结果非法或内存不足。
    pub fn bind_gateways(
        self,
        bindings: impl Iterator<Item = GatewayBinding>,
    ) -> Result<Vec<u8>, EncodeError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(self.bytes.len())
            .map_err(|_| EncodeError::NoMemory)?;
        bytes.extend_from_slice(self.bytes);
        bytes[8] = 1;
        let mut cursor = BATCH_HEADER + self.bridges.len() * 4;
        let mut bindings = bindings;
        for gateway in self.gateways() {
            let binding = bindings.next().ok_or(EncodeError::Invalid)?;
            let row = &mut bytes[cursor..cursor + GATEWAY];
            row[28..32].copy_from_slice(&binding.index.to_le_bytes());
            row[32..34].copy_from_slice(&binding.mtu.to_le_bytes());
            row[34] = u8::from(binding.carrier);
            cursor += GATEWAY
                + gateway.ipv6.len() * Ipv6Addresses::ROW_BYTES
                + gateway.advertise.dns().as_slice().len() * 16;
        }
        if bindings.next().is_some() {
            return Err(EncodeError::Invalid);
        }
        Ports::parse(&bytes)?;
        Ok(bytes)
    }

    /// 按桥号、VID 顺序借用本地接口配置。
    pub fn gateways(self) -> impl Iterator<Item = Gateway<'a>> + Clone {
        let mut tail = self.gateways;
        core::iter::from_fn(move || {
            let (gateway, next) = split_gateway(tail, false)?;
            tail = next;
            Some(gateway)
        })
    }

    /// 查找同一候选桥正文中的网关名称。未解析成员的身份为零。
    pub fn gateway(self, name: &[u8]) -> Option<Gateway<'a>> {
        self.gateways().find(|gateway| {
            gateway.name.get(..name.len()) == Some(name) && gateway.name.get(name.len()) == Some(&0)
        })
    }

    /// 按接口身份顺序遍历已验证的成员配置，不复制成员数组。
    pub fn iter(self) -> impl Iterator<Item = Port<'a>> {
        let mut tail = self.ports;
        core::iter::from_fn(move || {
            let (port, next) = split_port(tail)?;
            tail = next;
            Some(port)
        })
    }

    /// Bridge settings in 1-based bridge number order.
    pub fn bridges(self) -> impl ExactSizeIterator<Item = Bridge> + 'a {
        self.bridges.iter().map(|row| Bridge {
            priority: u16::from_le_bytes([row[0], row[1]]),
            rstp: row[2] != 0,
        })
    }

    /// 同一授权配置事务所传正文。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
}

pub(crate) fn encode<'a>(
    bridges: impl ExactSizeIterator<Item = &'a crate::configuration::Bridge> + Clone,
    ports: &[(u32, u16, u16, &crate::configuration::BridgePort)],
) -> Result<Vec<u8>, EncodeError> {
    if ports.len() > MAX_PORTS || bridges.len() > MAX_PORTS {
        return Err(EncodeError::Invalid);
    }
    let mut length = ports
        .iter()
        .try_fold(
            BATCH_HEADER + 4 * bridges.len(),
            |length, (_, _, _, port)| {
                length
                    .checked_add(HEADER)?
                    .checked_add(port.vlans.len().checked_mul(2)?)
            },
        )
        .ok_or(EncodeError::Invalid)?;
    let mut gateway_count = 0usize;
    for gateway in bridges.clone().flat_map(|bridge| bridge.gateways.values()) {
        gateway_count += 1;
        length = length
            .checked_add(
                GATEWAY
                    + gateway.ipv6.len() * Ipv6Addresses::ROW_BYTES
                    + gateway.advertise.dns().as_slice().len() * 16,
            )
            .ok_or(EncodeError::Invalid)?;
    }
    if gateway_count > MAX_PORTS {
        return Err(EncodeError::Invalid);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| EncodeError::NoMemory)?;
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&(bridges.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&(gateway_count as u16).to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    for bridge in bridges.clone() {
        bytes.extend_from_slice(&bridge.priority.to_le_bytes());
        bytes.extend_from_slice(&[u8::from(bridge.rstp), 0]);
    }
    for (at, bridge) in bridges.enumerate() {
        for (&vid, gateway) in &bridge.gateways {
            let start = bytes.len();
            let ipv6 = Ipv6Addresses::new(&gateway.ipv6).map_err(|_| EncodeError::Invalid)?;
            let dns_count = gateway.advertise.dns().as_slice().len();
            let dns_offset = GATEWAY + ipv6.len() * Ipv6Addresses::ROW_BYTES;
            bytes.resize(start + dns_offset + dns_count * 16, 0);
            let (row, dns_rows) = bytes[start..].split_at_mut(dns_offset);
            row[..2].copy_from_slice(&((at + 1) as u16).to_le_bytes());
            row[2..4].copy_from_slice(&vid.to_le_bytes());
            let name = gateway.name.as_str().as_bytes();
            row[4..4 + name.len()].copy_from_slice(name);
            row[20..24].copy_from_slice(&gateway.address.unwrap_or(Ipv4Addr::UNSPECIFIED).octets());
            row[24] = gateway.prefix_length;
            row[26..28].copy_from_slice(&(ipv6.len() as u16).to_le_bytes());
            ipv6.emit(&mut row[GATEWAY..]);
            row[40..44].copy_from_slice(&(dns_count as u32).to_le_bytes());
            emit_advertisement(gateway.advertise, &mut row[36..40], dns_rows);
        }
    }
    for &(index, bridge, number, port) in ports {
        bytes.extend_from_slice(&index.to_le_bytes());
        bytes.extend_from_slice(&bridge.to_le_bytes());
        bytes.extend_from_slice(&port.pvid.unwrap_or(0).to_le_bytes());
        let count = u16::try_from(port.vlans.len()).map_err(|_| EncodeError::Invalid)?;
        bytes.extend_from_slice(&count.to_le_bytes());
        let flags = u8::from(port.isolated)
            | (u8::from(port.edge) << 1)
            | (u8::from(port.point_to_point) << 2);
        bytes.extend_from_slice(&[flags, 0]);
        bytes.extend_from_slice(&number.to_le_bytes());
        bytes.extend_from_slice(&port.path_cost.to_le_bytes());
        for (&vid, &mode) in &port.vlans {
            let flags = if mode == crate::configuration::VlanEgress::Untagged {
                UNTAGGED
            } else {
                0
            };
            bytes.extend_from_slice(&(vid | flags).to_le_bytes());
        }
    }
    Ports::parse(&bytes)?;
    Ok(bytes)
}
