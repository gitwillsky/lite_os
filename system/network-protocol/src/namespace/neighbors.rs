//! 原生双栈邻居的冻结读投影；不携带 Linux NUD 位、时钟节点或写权限。
use crate::{SocketError, native_abi};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub(crate) const ORDINAL: u32 = 14;
const VERSION: u32 = 3;
const HEADER: usize = 8;
const ROW: usize = 32;

/// 已观察到的原生邻居可达状态，包含为网关选路保留的探测失败事实。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NeighborState {
    Incomplete,
    Reachable,
    Stale,
    Delay,
    Probe,
    Failed,
}

/// 一条邻居事实；逻辑接口标识与本族地址共同确定身份。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Neighbor {
    /// 原生逻辑接口身份；非零 u32，不按应用 ABI 截断。
    pub interface_index: u32,
    /// 当前记录的对端地址，身份还需包含所在接口。
    pub address: IpAddr,
    /// 只由有效 ARP/ND 学习的单播 MAC；解析未完成时为 None。
    pub hardware_address: Option<[u8; 6]>,
    pub state: NeighborState,
    /// 已接受的 IPv6 Router 位。IPv4 始终为 false。
    pub is_router: bool,
    /// 当前 INCOMPLETE/PROBE 阶段已成功发出的探测次数；其他状态为零。
    pub probes: u8,
}
impl Neighbor {
    fn valid(self) -> bool {
        self.interface_index != 0
            && match self.address {
                IpAddr::V4(address) => !self.is_router
                    && matches!(address.octets()[0], 1..=126 | 128..=223),
                IpAddr::V6(address) => !address.is_unspecified() && !address.is_multicast()
                    && !address.is_loopback() && address.to_ipv4_mapped().is_none(),
            }
            && matches!(
                self.state,
                NeighborState::Incomplete | NeighborState::Failed
            ) == self.hardware_address.is_none()
            && self
                .hardware_address
                .is_none_or(|mac| mac != [0; 6] && mac[0] & 1 == 0)
            && (matches!(self.state, NeighborState::Incomplete | NeighborState::Probe)
                || self.probes == 0)
    }
}

/// 一次 NET guard 中冻结的完整邻居集合；单一字节存储可经 VMO 交接，dump 不再复制行列表。
#[derive(Debug)]
pub struct NeighborSnapshot(Vec<u8>);
impl NeighborSnapshot {
    /// 冻结借用的实际行；clone 仅用于计数，不能在两次遍历间改变所属 NET。
    /// # Parameters
    /// `rows` 为同一 owner guard 下的稳定迭代器，不允许内部可变或伪造计数。
    /// # Returns
    /// 与迭代顺序一致的不可变快照，包括空集合。
    /// # Errors
    /// 字段非法、数量/长度溢出或分配失败；不发表部分结果。
    pub fn capture(rows: impl Iterator<Item = Neighbor> + Clone) -> Result<Self, SocketError> {
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
            output[4] = match row.state {
                NeighborState::Incomplete => 0,
                NeighborState::Reachable => 1,
                NeighborState::Stale => 2,
                NeighborState::Delay => 3,
                NeighborState::Probe => 4,
                NeighborState::Failed => 5,
            };
            output[5] = u8::from(row.is_router);
            output[6] = row.probes;
            output[7] = u8::from(row.hardware_address.is_some());
            match row.address {
                IpAddr::V4(address) => {
                    output[8..12].copy_from_slice(&address.octets());
                    output[30] = 4;
                }
                IpAddr::V6(address) => {
                    output[8..24].copy_from_slice(&address.octets());
                    output[30] = 6;
                }
            }
            if let Some(mac) = row.hardware_address {
                output[24..30].copy_from_slice(&mac);
            }
            written += 1;
        }
        if written != count as usize {
            return Err(SocketError::Invalid);
        }
        Ok(Self(bytes))
    }
    /// 捕获已交接 VMO 的字节，完整验证版本、边界和每条记录后才可读。
    /// # Errors
    /// 旧/未知版本、截断/尾随数据、保留位、隐藏字段或无效事实。
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
    /// 完整版本化正文；不包含可写原生对象或进程指针。
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    /// 冻结的实际条目数。
    pub fn len(&self) -> usize {
        (self.0.len() - HEADER) / ROW
    }
    /// 是否没有邻居。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// O(1) 读取一条已验证事实；越界返回 None，不分配。
    pub fn get(&self, index: usize) -> Option<Neighbor> {
        if index >= self.len() {
            return None;
        }
        Some(
            decode(&self.0[HEADER + index * ROW..HEADER + (index + 1) * ROW])
                .expect("immutable validated snapshot"),
        )
    }
}
fn decode(row: &[u8]) -> Result<Neighbor, native_abi::Status> {
    let invalid = native_abi::ERR_INVALID_ARGS;
    let state = match row[4] {
        0 => NeighborState::Incomplete,
        1 => NeighborState::Reachable,
        2 => NeighborState::Stale,
        3 => NeighborState::Delay,
        4 => NeighborState::Probe,
        5 => NeighborState::Failed,
        _ => return Err(invalid),
    };
    if row[5] > 1 || row[7] > 1 || row[31] != 0 || row[7] == 0 && row[24..30] != [0; 6] {
        return Err(invalid);
    }
    let address = match row[30] {
        4 if row[12..24] == [0; 12] => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(&row[8..12]).expect("IPv4 width"))),
        6 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&row[8..24]).expect("IPv6 width"))),
        _ => return Err(invalid),
    };
    let result = Neighbor {
        interface_index: u32::from_le_bytes(row[..4].try_into().expect("row width")),
        address,
        hardware_address: (row[7] == 1).then(|| row[24..30].try_into().expect("MAC width")),
        state,
        is_router: row[5] == 1,
        probes: row[6],
    };
    if !result.valid() {
        return Err(invalid);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    // 新 VMO framing 的保留位/宽度无法由 ND 状态测试验证；拒绝损坏正文必须先于逐行交付。
    #[test]
    fn neighbor_snapshot_preserves_native_identity_and_rejects_noncanonical_records() {
        let row = Neighbor {
            interface_index: u32::MAX,
            address: "fe80::2".parse().unwrap(),
            hardware_address: Some([2, 0, 0, 0, 0, 2]),
            state: NeighborState::Reachable,
            is_router: true,
            probes: 0,
        };
        let snapshot = NeighborSnapshot::capture([row].into_iter()).unwrap();
        assert_eq!(snapshot.get(0), Some(row));
        assert_eq!(snapshot.get(1), None);
        let encoded = snapshot.as_bytes();
        assert_eq!(encoded.len(), 40);
        assert_eq!(
            &encoded[..12],
            &[3, 0, 0, 0, 1, 0, 0, 0, 255, 255, 255, 255]
        );
        for (index, value) in [(0, 0), (4, 2), (12, 5), (13, 2), (14, 1), (15, 0), (38, 1)] {
            let mut bad = encoded.to_vec();
            bad[index] = value;
            assert!(NeighborSnapshot::parse(bad).is_err());
        }
        assert!(NeighborSnapshot::parse(encoded[..39].to_vec()).is_err());
        let mut tail = encoded.to_vec();
        tail.push(0);
        assert!(NeighborSnapshot::parse(tail).is_err());
        let empty = NeighborSnapshot::capture(core::iter::empty()).unwrap();
        assert!(
            NeighborSnapshot::parse(empty.as_bytes().to_vec())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            NeighborSnapshot::parse(encoded.to_vec()).unwrap().get(0),
            Some(row)
        );
        let ipv4 = Neighbor {
            address: "192.0.2.1".parse().unwrap(),
            is_router: false,
            ..row
        };
        let snapshot = NeighborSnapshot::capture([row, ipv4].into_iter()).unwrap();
        let encoded = snapshot.as_bytes();
        assert_eq!(NeighborSnapshot::parse(encoded.to_vec()).unwrap().get(1), Some(ipv4));
        for offset in [5, 12, 31] {
            let mut bad = encoded.to_vec();
            bad[HEADER + ROW + offset] = 1;
            assert!(NeighborSnapshot::parse(bad).is_err());
        }
    }
}
