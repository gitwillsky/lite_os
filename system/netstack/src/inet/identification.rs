//! 本机 IPv4/IPv6 分片标识按线上重组键隔离。排队期间持有编号，退休后才开始等待复用。
use core::net::{Ipv4Addr, Ipv6Addr};
use fallible_tree::{FallibleMap, VacantEntry};
use network_protocol::SocketError;
use ::tcp::time::{Duration, Instant};

// RFC 1122 的 120 秒 MDL；不能用本机较短的重组超时代替对端可能仍保留的分片寿命。
const IPV4_LIFETIME: Duration = Duration::from_millis(120_000);
const IPV6_LIFETIME: Duration = Duration::from_millis(60_000);
const MEMORY: usize = 4 * 1024 * 1024;
const BLOCKS: usize = 64;
const RETIRE_BUDGET: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    V4 {
        source: Ipv4Addr,
        destination: Ipv4Addr,
        protocol: u8,
    },
    V6 {
        source: Ipv6Addr,
        destination: Ipv6Addr,
    },
}
impl Key {
    fn block(self, value: u32) -> usize {
        (value
            >> match self {
                Self::V4 { .. } => 10,
                Self::V6 { .. } => 26,
            }) as usize
    }
    fn lifetime(self) -> Duration {
        match self {
            Self::V4 { .. } => IPV4_LIFETIME,
            Self::V6 { .. } => IPV6_LIFETIME,
        }
    }
    fn next(self, value: u32) -> u32 {
        match self {
            Self::V4 { .. } => u32::from((value as u16).wrapping_add(1).max(1)),
            Self::V6 { .. } => value.wrapping_add(1),
        }
    }
}

/// 唯一待发记录拥有的标识保留；短寿命元数据副本不拥有第二次释放权。
/// 不用 Drop 取得 NET 锁；所有出队、失败及关闭在同一 owner 内显式退休一次。
#[derive(Clone, Copy)]
pub(super) struct Lease {
    key: Key,
    value: u32,
}
impl Lease {
    pub(super) fn value(self) -> u32 {
        self.value
    }
    pub(super) fn ipv4_value(self) -> u16 {
        u16::try_from(self.value).expect("IPv4 identification lease")
    }
}

struct Flow {
    next: u32,
    // OWNER: 仅在跨块时检查整块的旧世代；没有它会把同块新编号误当成旧编号，或漏检回绕。
    entering: bool,
    // OWNER: 每块真实队列保留数；仍在排队的分片不能因时间经过而失去唯一性。
    active: [u32; BLOCKS],
    // OWNER: 每块最后一次退休后的 MDL。单调取最大值，乱序完成也不能提前复用。
    reusable: [Instant; BLOCKS],
}
impl Flow {
    fn new(seed: u32) -> Self {
        Self {
            next: seed,
            entering: true,
            active: [0; BLOCKS],
            reusable: [Instant::from_millis(0); BLOCKS],
        }
    }
    fn reserve(&mut self, key: Key, now: Instant) -> Result<Lease, SocketError> {
        let block = key.block(self.next);
        if self.entering && (self.active[block] != 0 || self.reusable[block] > now) {
            return Err(SocketError::NoBuffer);
        }
        let value = self.next;
        self.next = key.next(value);
        self.entering = key.block(self.next) != block;
        // 每世代只分配当前块内不同编号；跨世代之前整个块必须无人持有。
        self.active[block] = self.active[block]
            .checked_add(1)
            .expect("IP ID block reservation overflow");
        Ok(Lease { key, value })
    }
    fn expired(&self, now: Instant) -> bool {
        self.active.iter().all(|count| *count == 0)
            && self.reusable.iter().all(|deadline| *deadline <= now)
    }
}

/// 每个 NET 的唯一 IP 自动标识 owner；没有全局计数器、哈希碰撞或逐报文历史节点。
pub(super) struct Identification {
    flows: FallibleMap<Key, Flow>,
    // OWNER: 有界冷回收的扫描位置与时刻；空表无 timer，延迟回收不允许提前复用。
    retire_at: Option<Instant>,
    after: Option<Key>,
}
impl Identification {
    pub(super) fn new() -> Self {
        Self {
            flows: FallibleMap::new(),
            retire_at: None,
            after: None,
        }
    }
    /// 按线上 source/destination/protocol 保留编号；端口与接口不分割重组命名空间。
    /// # Errors
    /// 活跃/隔离编号或有界元数据耗尽返回 NoBuffer；新三元组节点 OOM 返回 NoMemory。
    pub(super) fn reserve(
        &mut self,
        source: Ipv4Addr,
        destination: Ipv4Addr,
        protocol: u8,
        now: Instant,
    ) -> Result<Lease, SocketError> {
        self.reserve_key(
            Key::V4 {
                source,
                destination,
                protocol,
            },
            now,
        )
    }

    /// 为 IPv6 源和目的保留 32 位编号。不同上层协议共享同一重组空间。
    /// # Errors
    /// 同 IPv4，容量不足返回 NoBuffer，节点分配失败返回 NoMemory。
    pub(super) fn reserve_ipv6(
        &mut self,
        source: Ipv6Addr,
        destination: Ipv6Addr,
        now: Instant,
    ) -> Result<Lease, SocketError> {
        self.reserve_key(
            Key::V6 {
                source,
                destination,
            },
            now,
        )
    }

    fn reserve_key(&mut self, key: Key, now: Instant) -> Result<Lease, SocketError> {
        if let Some(flow) = self.flows.get_mut(&key) {
            return flow.reserve(key, now);
        }
        if self.flows.len() >= MEMORY / VacantEntry::<Key, Flow>::allocation_size() {
            return Err(SocketError::NoBuffer);
        }
        // RFC 7739 §5.1 的随机初值按目的隔离；只在创建三元组时取熵，稳态不调用服务。
        let mut seed = [0; 4];
        native_runtime::system::cprng_draw(&mut seed);
        let seed = u32::from_ne_bytes(seed);
        let mut flow = Flow::new(match key {
            Key::V4 { .. } => u32::from((seed as u16).max(1)),
            Key::V6 { .. } => seed,
        });
        let lease = flow.reserve(key, now)?;
        let node = FallibleMap::try_prepare(key, flow).map_err(|_| SocketError::NoMemory)?;
        self.flows.commit_vacant(node);
        self.retire_at.get_or_insert(now + IPV6_LIFETIME);
        Ok(lease)
    }

    /// 为已验证的本机 IPv4 包分配并写入编号，按实际头长度更新 checksum。
    /// # Errors
    /// 同 `reserve`；失败保持报文字节不变。调用方负责已验证报头与自动编号策略。
    pub(super) fn reserve_header(
        &mut self,
        packet: &mut [u8],
        now: Instant,
    ) -> Result<Lease, SocketError> {
        let source = Ipv4Addr::from(<[u8; 4]>::try_from(&packet[12..16]).unwrap());
        let destination = Ipv4Addr::from(<[u8; 4]>::try_from(&packet[16..20]).unwrap());
        let lease = self.reserve(source, destination, packet[9], now)?;
        packet[4..6].copy_from_slice(&lease.ipv4_value().to_be_bytes());
        packet[10..12].fill(0);
        let header = usize::from(packet[0] & 15) * 4;
        let checksum = packet_wire::checksum::checksum(&packet[..header]);
        packet[10..12].copy_from_slice(&checksum.to_be_bytes());
        Ok(lease)
    }

    /// 唯一队列记录退休时调用一次；失败/取消也保守隔离，成功的最后一片不早于此时。
    pub(super) fn release(&mut self, lease: Lease, now: Instant) {
        let flow = self.flows.get_mut(&lease.key).expect("live IP ID lease");
        let block = lease.key.block(lease.value);
        flow.active[block] = flow.active[block]
            .checked_sub(1)
            .expect("IP ID released twice");
        flow.reusable[block] = flow.reusable[block].max(now + lease.key.lifetime());
    }
    pub(super) fn due(&self, now: Instant) -> bool {
        self.retire_at.is_some_and(|at| at <= now)
    }
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.retire_at
    }
    /// 最多扫描固定数量的三元组；仍保有记录或 MDL 未过的状态绝不因容量压力驱逐。
    pub(super) fn retire(&mut self, now: Instant) -> bool {
        if !self.due(now) {
            return false;
        }
        for _ in 0..RETIRE_BUDGET {
            let next = match self.after {
                Some(key) => self.flows.successor(&key),
                None => self.flows.first_key_value(),
            }
            .map(|(&key, flow)| (key, flow.expired(now)));
            let Some((key, expired)) = next else {
                self.after = None;
                self.retire_at = (!self.flows.is_empty()).then_some(now + IPV6_LIFETIME);
                return false;
            };
            self.after = Some(key);
            if expired {
                self.flows.remove(&key);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 既有少量分片用例达不到 16-bit 回绕，也不能覆盖排队跨 MDL 的编号保留。
    #[test]
    fn wrap_never_reuses_live_or_recent_ids_and_other_tuples_remain_independent() {
        let mut ids = Identification::new();
        let source = Ipv4Addr::new(192, 0, 2, 1);
        let destination = Ipv4Addr::new(192, 0, 2, 2);
        let key = Key::V4 {
            source,
            destination,
            protocol: 17,
        };
        let now = Instant::from_millis(1);
        ids.flows
            .commit_vacant(FallibleMap::try_prepare(key, Flow::new(1)).unwrap());
        ids.retire_at = Some(now + IPV4_LIFETIME);
        let held = ids.reserve(source, destination, 17, now).unwrap();
        for expected in 2..=u16::MAX {
            let lease = ids.reserve(source, destination, 17, now).unwrap();
            assert_eq!(lease.ipv4_value(), expected);
            ids.release(lease, now);
        }
        assert!(matches!(
            ids.reserve(source, destination, 17, now),
            Err(SocketError::NoBuffer)
        ));
        let later = now + IPV4_LIFETIME;
        assert!(!ids.retire(later));
        assert!(matches!(
            ids.reserve(source, destination, 17, later),
            Err(SocketError::NoBuffer)
        ));
        let other = ids.reserve(source, destination, 1, later).unwrap();
        ids.release(other, later);
        ids.release(held, later);
        assert!(matches!(
            ids.reserve(source, destination, 17, later),
            Err(SocketError::NoBuffer)
        ));
        let next = ids
            .reserve(source, destination, 17, later + IPV4_LIFETIME)
            .unwrap();
        assert_eq!(next.value(), 1);
        ids.release(next, later + IPV4_LIFETIME);
        assert!(!ids.retire(later + IPV4_LIFETIME));
        assert!(ids.flows.contains_key(&key));
        assert!(!ids.retire(later + IPV4_LIFETIME + IPV4_LIFETIME));
        assert!(ids.flows.is_empty());
        assert!(!ids.due(later + IPV4_LIFETIME + IPV4_LIFETIME));
    }
}
