//! 带权 hash-threshold：只访问所属 FIB 的路径描述和本次 guard 中的实际设备事实。
use super::*;
use network_protocol::routes::{NextHopConfiguration, NextHopMember};

pub(super) struct Group {
    members: Vec<NextHopMember>,
}
impl Group {
    pub(super) fn capture(definition: NextHopConfiguration<'_>) -> Result<Self, SocketError> {
        u32::try_from(definition.len()).map_err(|_| SocketError::NoSpace)?;
        let mut members = Vec::new();
        members
            .try_reserve_exact(definition.len())
            .map_err(|_| SocketError::NoMemory)?;
        for member in definition.members() {
            if member.weight == 0 {
                return Err(SocketError::Invalid);
            }
            members.push(member);
        }
        // 组是路径集合；规范顺序使仅重排输入不改变流映射，不允许重复路径暗中加权。
        members.sort_unstable_by_key(|member| {
            (member.interface_index, member.gateway, member.on_link)
        });
        if members.windows(2).any(|pair| {
            (pair[0].interface_index, pair[0].gateway, pair[0].on_link)
                == (pair[1].interface_index, pair[1].gateway, pair[1].on_link)
        }) {
            return Err(SocketError::Invalid);
        }
        Ok(Self { members })
    }
    pub(super) fn members(&self) -> &[NextHopMember] {
        &self.members
    }
    pub(super) fn retire(&mut self, index: u32, force: bool) {
        // HOST 路由只允许单成员；软 DOWN 保留其描述，非 HOST 前缀由 FIB 退休。
        // 多成员组没有 HOST 引用，可永久移除失效成员，后续 UP 不自动复活旧路径。
        if force || self.members.len() != 1 {
            self.members
                .retain(|member| member.interface_index != index);
        }
    }
    pub(super) fn remove_missing(&mut self, interfaces: &[NetworkInterface]) {
        self.members.retain(|member| {
            interfaces
                .iter()
                .any(|interface| interface.index == member.interface_index)
        });
    }
}
fn eligible(
    member: NextHopMember,
    bound: Option<u32>,
    kind: Ipv4RouteKind,
    interfaces: impl Iterator<Item = InterfaceFacts>,
) -> bool {
    bound.is_none_or(|index| member.interface_index == index)
        && interfaces
            .into_iter()
            .find(|interface| interface.index == member.interface_index)
            .is_some_and(|interface| {
                interface.mtu >= 68 && (interface.up || kind == Ipv4RouteKind::Local)
            })
}
/// 源/目的地址键与 Linux 默认 L3 ECMP 一致；分片不依赖只在首片存在的端口。
/// Jenkins lookup3 的 two-word final，seed 只分隔组身份，不声称是抗攻击密钥。
fn hash(source: Ipv4Addr, destination: Ipv4Addr, seed: u64) -> u32 {
    let init = 0xdead_beef_u32
        .wrapping_add(8)
        .wrapping_add(seed as u32 ^ (seed >> 32) as u32);
    let mut a = u32::from(source).wrapping_add(init);
    let mut b = u32::from(destination).wrapping_add(init);
    let mut c = init;
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(14));
    a ^= c;
    a = a.wrapping_sub(c.rotate_left(11));
    b ^= a;
    b = b.wrapping_sub(a.rotate_left(25));
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(16));
    a ^= c;
    a = a.wrapping_sub(c.rotate_left(4));
    b ^= a;
    b = b.wrapping_sub(a.rotate_left(14));
    c ^= b;
    c.wrapping_sub(b.rotate_left(24))
}
pub(super) fn select(
    members: &[NextHopMember],
    flow: Flow,
    seed: u64,
    kind: Ipv4RouteKind,
    interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
) -> Option<NextHopMember> {
    if let [member] = members {
        return eligible(*member, flow.bound, kind, interfaces).then_some(*member);
    }
    // 成员数和每个权重都不超过 u32::MAX，总权重不溢出 u64；两遍读取由同一 NET guard 保证一致。
    // capture 已按出口排列；一次读取同一出口事实即可裁决整个连续路径组，不复制可用性状态。
    let runs = || {
        members
            .chunk_by(|left, right| left.interface_index == right.interface_index)
            .filter(|run| eligible(run[0], flow.bound, kind, interfaces.clone()))
    };
    let total: u64 = runs()
        .flat_map(|run| run.iter())
        .map(|member| u64::from(member.weight))
        .sum();
    if total == 0 {
        return None;
    }
    let point =
        ((u128::from(hash(flow.source, flow.destination, seed)) * u128::from(total)) >> 32) as u64;
    let mut cumulative = 0u64;
    for run in runs() {
        for member in run {
            cumulative += u64::from(member.weight);
            if point < cumulative {
                return Some(*member);
            }
        }
    }
    unreachable!("weighted next-hop selection escaped the eligible interval")
}
