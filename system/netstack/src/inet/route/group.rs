//! 带权 hash-threshold：只访问所属 FIB 的路径描述和本次 guard 中的实际设备事实。
use super::*;
use network_protocol::routes::{NextHopConfiguration, NextHopMember};

pub(super) struct Group<A: network_protocol::routes::RouteAddress> {
    members: Vec<NextHopMember<A>>,
}
impl<A: network_protocol::routes::RouteAddress> Group<A> {
    pub(super) fn capture(definition: NextHopConfiguration<'_, A>) -> Result<Self, SocketError> {
        if definition.is_empty() {
            return Err(SocketError::Invalid);
        }
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
    pub(super) fn members(&self) -> &[NextHopMember<A>] {
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
fn eligible_mtu(
    index: u32,
    bound: Option<u32>,
    kind: RouteKind,
    interfaces: impl Iterator<Item = InterfaceFacts>,
) -> Option<usize> {
    if bound.is_some_and(|bound| index != bound) {
        return None;
    }
    interfaces
        .into_iter()
        .find(|interface| interface.index == index)
        .filter(|interface| interface.mtu >= 68 && (interface.up || kind == RouteKind::Local))
        .map(|interface| interface.mtu)
}
/// 源/目的地址键与 Linux 默认 L3 ECMP 一致；分片不依赖只在首片存在的端口。
/// Jenkins lookup3 的 two-word final，seed 只分隔组身份，不声称是抗攻击密钥。
fn hash(source: Ipv4Addr, destination: Ipv4Addr, seed: u64) -> u32 {
    let init = 0xdead_beef_u32
        .wrapping_add(8)
        .wrapping_add(seed as u32 ^ (seed >> 32) as u32);
    let a = u32::from(source).wrapping_add(init);
    let b = u32::from(destination).wrapping_add(init);
    let c = init;
    finish(a, b, c)
}
fn finish(mut a: u32, mut b: u32, mut c: u32) -> u32 {
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
    members: &[NextHopMember<core::net::Ipv4Addr>],
    flow: Flow,
    seed: u64,
    kind: RouteKind,
    interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
) -> Option<NextHopMember<core::net::Ipv4Addr>> {
    let mtu = |index| eligible_mtu(index, flow.bound, kind, interfaces.clone());
    let usable = |member: &NextHopMember<Ipv4Addr>, mtu| {
        mtu >= 1280 || !member.gateway.is_some_and(|gateway| gateway.is_ipv6())
    };
    if let [member] = members {
        return mtu(member.interface_index)
            .filter(|mtu| usable(member, *mtu))
            .map(|_| *member);
    }
    // 成员数与权重均不超过 u32::MAX；同出口连续排列，资格检查不按每个网关重复。
    choose(
        || {
            members
                .chunk_by(|left, right| left.interface_index == right.interface_index)
                .filter_map(|run| mtu(run[0].interface_index).map(|mtu| (run, mtu)))
                .flat_map(|(run, mtu)| run.iter().filter(move |member| usable(member, mtu)))
        },
        || hash(flow.source, flow.destination, seed),
    )
}

pub(super) fn select_members<A: network_protocol::routes::RouteAddress>(
    members: &[NextHopMember<A>],
    hash: impl FnOnce() -> u32,
    eligible: impl Fn(&NextHopMember<A>) -> bool,
) -> Option<NextHopMember<A>> {
    if let [member] = members {
        return eligible(member).then_some(*member);
    }
    choose(|| members.iter().filter(|member| eligible(member)), hash)
}

fn choose<
    'a,
    A: network_protocol::routes::RouteAddress + 'a,
    I: Iterator<Item = &'a NextHopMember<A>>,
>(
    members: impl Fn() -> I,
    hash: impl FnOnce() -> u32,
) -> Option<NextHopMember<A>> {
    let total: u64 = members().map(|member| u64::from(member.weight)).sum();
    if total == 0 {
        return None;
    }
    let point = ((u128::from(hash()) * u128::from(total)) >> 32) as u64;
    let mut cumulative = 0u64;
    for member in members() {
        cumulative += u64::from(member.weight);
        if point < cumulative {
            return Some(*member);
        }
    }
    unreachable!("weighted next-hop selection escaped the eligible interval")
}

/// Jenkins lookup3/jhash2 的八字 L3 键；两个 IPv6 地址的全部位参与，不折叠高低半部。
pub(super) fn hash_ipv6(
    source: core::net::Ipv6Addr,
    destination: core::net::Ipv6Addr,
    seed: u64,
) -> u32 {
    let source = source.octets();
    let destination = destination.octets();
    let mut words = [0; 8];
    for (word, bytes) in words.iter_mut().zip(
        source
            .as_chunks::<4>()
            .0
            .iter()
            .chain(destination.as_chunks::<4>().0),
    ) {
        *word = u32::from_be_bytes(*bytes);
    }
    let init = 0xdead_beef_u32
        .wrapping_add(32)
        .wrapping_add(seed as u32 ^ (seed >> 32) as u32);
    let (mut a, mut b, mut c) = (init, init, init);
    for offset in [0, 3] {
        a = a.wrapping_add(words[offset]);
        b = b.wrapping_add(words[offset + 1]);
        c = c.wrapping_add(words[offset + 2]);
        a = a.wrapping_sub(c);
        a ^= c.rotate_left(4);
        c = c.wrapping_add(b);
        b = b.wrapping_sub(a);
        b ^= a.rotate_left(6);
        a = a.wrapping_add(c);
        c = c.wrapping_sub(b);
        c ^= b.rotate_left(8);
        b = b.wrapping_add(a);
        a = a.wrapping_sub(c);
        a ^= c.rotate_left(16);
        c = c.wrapping_add(b);
        b = b.wrapping_sub(a);
        b ^= a.rotate_left(19);
        a = a.wrapping_add(c);
        c = c.wrapping_sub(b);
        c ^= b.rotate_left(4);
        b = b.wrapping_add(a);
    }
    finish(a.wrapping_add(words[6]), b.wrapping_add(words[7]), c)
}
