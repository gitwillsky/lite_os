//! 原生双栈邻居的冻结读投影；不携带 Linux NUD 位、时钟节点或写权限。
use crate::{SocketError, native_abi};
use alloc::vec::Vec;
use router_protocol::neighbors::ROW_BYTES as ROW;
pub use router_protocol::neighbors::{Neighbor, NeighborState};

pub(crate) const ORDINAL: u32 = 14;
const VERSION: u32 = 3;
const HEADER: usize = 8;

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
            if written == count as usize {
                return Err(SocketError::Invalid);
            }
            let output = &mut bytes[HEADER + written * ROW..HEADER + (written + 1) * ROW];
            output.copy_from_slice(&row.encode().map_err(|_| SocketError::Invalid)?);
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
            Neighbor::decode(row).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
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
            Neighbor::decode(&self.0[HEADER + index * ROW..HEADER + (index + 1) * ROW])
                .expect("immutable validated snapshot"),
        )
    }
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
        assert_eq!(
            NeighborSnapshot::parse(encoded.to_vec()).unwrap().get(1),
            Some(ipv4)
        );
        for offset in [5, 12, 31] {
            let mut bad = encoded.to_vec();
            bad[HEADER + ROW + offset] = 1;
            assert!(NeighborSnapshot::parse(bad).is_err());
        }
    }
}
