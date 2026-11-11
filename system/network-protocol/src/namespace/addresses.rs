//! 实际接口地址的冻结读投影；不把多地址列表放入每次收发都使用的轻量接口事实。
use crate::{SocketError, native_abi};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub(crate) const ORDINAL: u32 = 15;
const VERSION: u32 = 6;
const HEADER: usize = 8;
const ROW: usize = 56;

/// 地址当前使用资格；Preferred 和 Deprecated 可用于收发，选源优先 Preferred。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressState {
    Tentative,
    Preferred,
    Duplicate,
    Deprecated,
}
/// 地址配置的职责来源；不以地址值推断，也不等同于 Linux 地址 flags。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressOrigin {
    /// 接口本身维护的内建地址；替换外部地址配置时保留。
    Interface,
    /// 经配置安装的地址，包括静态地址与配置服务应用的租约。
    Configured,
    /// 由收到的路由通告自动取得。
    Slaac,
}
/// 一条原生地址事实；接口、地址及前缀是实际 owner 的观察，不包含 Linux flags 或写权限。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterfaceAddress {
    pub interface_index: u32,
    /// 接口当前名称，用于标准地址标签；不是持久配置身份。
    pub name: [u8; 16],
    pub address: IpAddr,
    pub prefix_length: u8,
    pub state: AddressState,
    /// 同一 NET guard 下冻结的发包资格，包含地址状态、接口启用、链路可用与最小 MTU。
    /// 缺少此事实会让控制协议拼接不同时刻的接口与地址快照。
    pub usable: bool,
    pub origin: AddressOrigin,
    /// 此地址是否显式建立同前缀直连意图；不等同于邻居已经可达。
    pub on_link: bool,
    /// 仅 IPv4 地址可带实际广播地址，不从前缀猜测。
    pub broadcast: Option<Ipv4Addr>,
    /// 捕获时的剩余优选期（秒），u32::MAX 表示无限。
    pub preferred_lifetime: u32,
    /// 捕获时的剩余有效期（秒），u32::MAX 表示无限。
    pub valid_lifetime: u32,
}
impl InterfaceAddress {
    fn valid(self) -> bool {
        self.interface_index != 0
            && !self.address.is_unspecified()
            && self.valid_lifetime != 0
            && self.preferred_lifetime <= self.valid_lifetime
            && match self.origin {
                AddressOrigin::Slaac => match self.state {
                    AddressState::Preferred => self.preferred_lifetime != 0,
                    AddressState::Deprecated => self.preferred_lifetime == 0,
                    _ => true,
                },
                _ => {
                    self.preferred_lifetime == u32::MAX
                        && self.valid_lifetime == u32::MAX
                        && self.state != AddressState::Deprecated
                }
            }
            && (!self.usable
                || matches!(
                    self.state,
                    AddressState::Preferred | AddressState::Deprecated
                ))
            && match self.address {
                IpAddr::V4(_) => {
                    self.prefix_length <= 32
                        && self.state == AddressState::Preferred
                        && self.origin != AddressOrigin::Slaac
                }
                IpAddr::V6(address) => {
                    self.prefix_length <= 128
                        && self.broadcast.is_none()
                        && !address.is_multicast()
                        && address.to_ipv4_mapped().is_none()
                }
            }
    }
}
/// 一次 NET guard 内冻结的地址集合；只持一份正文，经 VMO 交接后可逐行读取。
#[derive(Debug)]
pub struct AddressSnapshot(Vec<u8>);
impl AddressSnapshot {
    /// 捕获稳定 owner 迭代器；clone 只计数，不允许两次遍历之间改变 NET。
    /// # Errors
    /// 字段、数量或长度非法、存储不足；失败不发布部分集合。
    pub fn capture(
        rows: impl Iterator<Item = InterfaceAddress> + Clone,
    ) -> Result<Self, SocketError> {
        let count = rows.clone().count();
        let length = count
            .checked_mul(ROW)
            .and_then(|n| n.checked_add(HEADER))
            .ok_or(SocketError::Invalid)?;
        let count = u32::try_from(count).map_err(|_| SocketError::Invalid)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| SocketError::NoMemory)?;
        bytes.resize(length, 0);
        bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
        bytes[4..8].copy_from_slice(&count.to_le_bytes());
        let mut written = 0;
        for row in rows {
            if written == count as usize || !row.valid() {
                return Err(SocketError::Invalid);
            }
            let output = &mut bytes[HEADER + written * ROW..HEADER + (written + 1) * ROW];
            output[..4].copy_from_slice(&row.interface_index.to_le_bytes());
            output[4] = match row.address {
                IpAddr::V4(address) => {
                    output[8..12].copy_from_slice(&address.octets());
                    4
                }
                IpAddr::V6(address) => {
                    output[8..24].copy_from_slice(&address.octets());
                    6
                }
            };
            output[5] = row.prefix_length;
            output[6] = match row.state {
                AddressState::Tentative => 0,
                AddressState::Preferred => 1,
                AddressState::Duplicate => 2,
                AddressState::Deprecated => 3,
            };
            output[7] = u8::from(row.broadcast.is_some())
                | (u8::from(row.on_link) << 1)
                | (u8::from(row.origin == AddressOrigin::Interface) << 2)
                | (u8::from(row.usable) << 3)
                | (u8::from(row.origin == AddressOrigin::Slaac) << 4);
            if let Some(broadcast) = row.broadcast {
                output[24..28].copy_from_slice(&broadcast.octets());
            }
            output[28..44].copy_from_slice(&row.name);
            output[48..52].copy_from_slice(&row.preferred_lifetime.to_le_bytes());
            output[52..56].copy_from_slice(&row.valid_lifetime.to_le_bytes());
            written += 1;
        }
        if written != count as usize {
            return Err(SocketError::Invalid);
        }
        Ok(Self(bytes))
    }
    /// 验证交接的完整正文后接管；不保留另一份地址行数组。
    /// # Errors
    /// 版本、长度/尾随、族、状态、保留字段或行非法。
    pub fn parse(bytes: Vec<u8>) -> Result<Self, native_abi::Status> {
        let header = bytes.get(..HEADER).ok_or(native_abi::ERR_INVALID_ARGS)?;
        let version = u32::from_le_bytes(header[..4].try_into().expect("header width"));
        let count = u32::from_le_bytes(header[4..8].try_into().expect("header width")) as usize;
        if version != VERSION
            || count.checked_mul(ROW).and_then(|n| n.checked_add(HEADER)) != Some(bytes.len())
        {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        for row in bytes[HEADER..].as_chunks::<ROW>().0 {
            decode(row)?;
        }
        Ok(Self(bytes))
    }
    /// 完整版本化正文。
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    /// 当前被冻结的行数。
    pub fn len(&self) -> usize {
        (self.0.len() - HEADER) / ROW
    }
    /// 是否没有地址。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// O(1) 读取指定事实；越界返回 None，无分配或重新查询。
    pub fn get(&self, index: usize) -> Option<InterfaceAddress> {
        (index < self.len()).then(|| {
            decode(&self.0[HEADER + index * ROW..HEADER + (index + 1) * ROW])
                .expect("validated address")
        })
    }
}
fn decode(row: &[u8]) -> Result<InterfaceAddress, native_abi::Status> {
    let invalid = native_abi::ERR_INVALID_ARGS;
    let address = match row[4] {
        4 if row[12..24] == [0; 12] => IpAddr::V4(Ipv4Addr::from(
            <[u8; 4]>::try_from(&row[8..12]).expect("IPv4 width"),
        )),
        6 => IpAddr::V6(Ipv6Addr::from(
            <[u8; 16]>::try_from(&row[8..24]).expect("IPv6 width"),
        )),
        _ => return Err(invalid),
    };
    let state = match row[6] {
        0 => AddressState::Tentative,
        1 => AddressState::Preferred,
        2 => AddressState::Duplicate,
        3 => AddressState::Deprecated,
        _ => return Err(invalid),
    };
    if row[7] & !31 != 0
        || row[7] & 20 == 20
        || row[7] & 1 == 0 && row[24..28] != [0; 4]
        || row[44..48] != [0; 4]
    {
        return Err(invalid);
    }
    let result = InterfaceAddress {
        interface_index: u32::from_le_bytes(row[..4].try_into().expect("index width")),
        name: row[28..44].try_into().expect("name width"),
        address,
        prefix_length: row[5],
        state,
        usable: row[7] & 8 != 0,
        origin: if row[7] & 4 != 0 {
            AddressOrigin::Interface
        } else if row[7] & 16 != 0 {
            AddressOrigin::Slaac
        } else {
            AddressOrigin::Configured
        },
        on_link: row[7] & 2 != 0,
        broadcast: (row[7] & 1 != 0)
            .then(|| Ipv4Addr::from(<[u8; 4]>::try_from(&row[24..28]).expect("broadcast width"))),
        preferred_lifetime: u32::from_le_bytes(row[48..52].try_into().expect("lifetime width")),
        valid_lifetime: u32::from_le_bytes(row[52..56].try_into().expect("lifetime width")),
    };
    if !result.valid() {
        return Err(invalid);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn address_snapshot_preserves_both_families_and_rejects_ambiguous_records() {
        let rows = [
            InterfaceAddress {
                interface_index: 1,
                name: [0; 16],
                address: Ipv4Addr::new(192, 0, 2, 1).into(),
                prefix_length: 24,
                state: AddressState::Preferred,
                usable: true,
                origin: AddressOrigin::Configured,
                on_link: true,
                broadcast: Some(Ipv4Addr::new(192, 0, 2, 255)),
                preferred_lifetime: u32::MAX,
                valid_lifetime: u32::MAX,
            },
            InterfaceAddress {
                interface_index: u32::MAX,
                name: [0; 16],
                address: Ipv6Addr::LOCALHOST.into(),
                prefix_length: 128,
                state: AddressState::Preferred,
                usable: true,
                origin: AddressOrigin::Interface,
                on_link: true,
                broadcast: None,
                preferred_lifetime: u32::MAX,
                valid_lifetime: u32::MAX,
            },
        ];
        let snapshot = AddressSnapshot::capture(rows.into_iter()).unwrap();
        let frozen = AddressSnapshot::parse(snapshot.as_bytes().to_vec()).unwrap();
        assert_eq!(frozen.len(), 2);
        assert_eq!(frozen.get(0), Some(rows[0]));
        assert_eq!(frozen.get(1), Some(rows[1]));
        assert_eq!(frozen.get(2), None);
        for (offset, value) in [
            (0, 3),
            (4, 3),
            (12, 5),
            (13, 33),
            (14, 0),
            (15, 16),
            (20, 1),
            (52, 1),
            (60, 1),
            (71, 1),
            (70, 0),
        ] {
            let mut bad = snapshot.as_bytes().to_vec();
            bad[offset] = value;
            assert!(AddressSnapshot::parse(bad).is_err(), "offset {offset}");
        }
        for length in [0, HEADER - 1, HEADER, snapshot.as_bytes().len() - 1] {
            assert!(AddressSnapshot::parse(snapshot.as_bytes()[..length].to_vec()).is_err());
        }
        let empty = AddressSnapshot::capture(core::iter::empty()).unwrap();
        assert!(
            AddressSnapshot::parse(empty.as_bytes().to_vec())
                .unwrap()
                .is_empty()
        );
    }
}
