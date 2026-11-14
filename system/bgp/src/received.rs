//! RFC 7606 的整份 UPDATE 裁决；先检查所有区间/属性，再交出宣告或撤回，不在途中修改 RIB。
use super::{Config, Negotiated, Notice, Path};
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use packet_wire::bgp::{
    attributes::{AsPath, AsnWidth, Attribute, SegmentKind, Value},
    nlri::{Family, Nlri, Prefix},
    update::Update,
};

/// 错误处理的整批结果；TreatAsWithdraw 不是忽略报文，所有可辨认的宣告都必须撤回。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handling {
    Accept,
    TreatAsWithdraw,
}
/// 有效聚合器；Partial 是协议语义，不能在重建/转发时丢弃。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Aggregator {
    pub asn: u32,
    pub identifier: Ipv4Addr,
    pub partial: bool,
}
/// BGP 宣告的逻辑下一跳；链路范围的接口绑定及可达性仍须由 transport/RIB owner 证明。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NextHop {
    pub address: IpAddr,
    pub link_local: Option<Ipv6Addr>,
}
/// 一条本邻居的增量决定；宣告覆盖对相同前缀及路径标识的显式撤回。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Withdraw(Prefix, Option<u32>),
    Announce {
        prefix: Prefix,
        path_id: Option<u32>,
        next_hop: NextHop,
    },
}

// OWNER: 一次 UPDATE 的 256 个属性编号集合。固定 32 bytes，无字典节点或逐属性分配；
// 不记录它会使重复项覆盖首项、丢弃错误属性后又被后续副本重新接纳。
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Codes([u64; 4]);
impl Codes {
    fn contains(self, code: u8) -> bool {
        self.0[usize::from(code) / 64] & (1 << (code % 64)) != 0
    }
    fn insert(&mut self, code: u8) -> bool {
        let present = self.contains(code);
        self.0[usize::from(code) / 64] |= 1 << (code % 64);
        !present
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section<'a> {
    Withdraw(Nlri<'a>),
    Announce(Nlri<'a>, Option<NextHop>),
}

/// 同一次输入的完整错误裁决；原包/属性/NLRI 只借用输入，不建立前缀或属性数组。
/// 已完成 AS4 重建；仍不等于通过入口策略、去环、最佳路径或下一跳可达性准入。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceivedUpdate<'a> {
    raw: &'a [u8],
    update: Update<'a>,
    sections: [Option<Section<'a>>; 4],
    retained: Codes,
    errors: Codes,
    handling: Handling,
    framing_error: bool,
    end_of_rib: Option<Family>,
    path: Option<Path<'a>>,
    aggregator: Option<Aggregator>,
}
impl<'a> ReceivedUpdate<'a> {
    /// 整批接受或按撤回处理；会话重置通过 capture 的 Err 交给 Session。
    pub fn handling(&self) -> Handling {
        self.handling
    }
    /// 原始完整 BGP 消息，供 RFC 7606 的诊断留证，不隐式把日志写入协议热路径。
    pub fn message_bytes(&self) -> &'a [u8] {
        self.raw
    }
    /// 本批是否有属性 framing 截断；原始报文与已辨认的 NLRI 仍被保留。
    pub fn framing_error(&self) -> bool {
        self.framing_error
    }
    /// 按编号读取错误、缺失或不应携带的已知属性，不构造诊断数组。
    pub fn attribute_errors(&self) -> impl Iterator<Item = u8> + '_ {
        (0u16..=255)
            .map(|code| code as u8)
            .filter(|code| self.errors.contains(*code))
    }
    /// 只有完全符合该族 EoR 布局时返回标记；不据此实现 Graceful Restart 或跨重启恢复。
    pub fn end_of_rib(&self) -> Option<Family> {
        self.end_of_rib
    }
    /// 错误分类后保留的原属性首项；仍须策略/传播处理，不可作为最终 Loc-RIB 属性。
    /// 路径和聚合器统一经有效值访问器读取，原始及 AS4 辅助属性均不在此序列传播。
    pub fn received_attributes(&self) -> impl Iterator<Item = Attribute<'a>> {
        let mut scan = self.update.scan_attributes();
        let mut seen = Codes::default();
        let retained = self.retained;
        let enabled = self.handling == Handling::Accept;
        core::iter::from_fn(move || {
            if !enabled {
                return None;
            }
            loop {
                let attribute = scan.next()?.ok()?;
                if seen.insert(attribute.code)
                    && retained.contains(attribute.code)
                    && !matches!(attribute.code, 2 | 7 | 17 | 18)
                {
                    return Some(attribute);
                }
            }
        })
    }
    /// 完成 AS4 重建的有效路径；按撤回处理或没有路径属性时返回 None。
    pub fn path(&self) -> Option<Path<'a>> {
        (self.handling == Handling::Accept)
            .then_some(self.path)
            .flatten()
    }
    /// 按 RFC 6793 覆盖规则选定的聚合器；按撤回处理或属性缺失时返回 None。
    pub fn aggregator(&self) -> Option<Aggregator> {
        (self.handling == Handling::Accept)
            .then_some(self.aggregator)
            .flatten()
    }
    /// 先交付全部显式撤回，再交付宣告或其等价撤回；调用者必须把整批决定应用后才发布新 RIB。
    pub fn changes(&self) -> impl Iterator<Item = Change> {
        let withdraw_all = self.handling == Handling::TreatAsWithdraw;
        self.sections
            .into_iter()
            .flatten()
            .flat_map(move |section| {
                let (nlri, next_hop) = match section {
                    Section::Withdraw(nlri) => (nlri, None),
                    Section::Announce(nlri, hop) => (nlri, if withdraw_all { None } else { hop }),
                };
                nlri.paths().map(move |(prefix, path_id)| match next_hop {
                    Some(next_hop) => Change::Announce {
                        prefix,
                        path_id,
                        next_hop,
                    },
                    None => Change::Withdraw(prefix, path_id),
                })
            })
    }
}

fn unicast(address: Ipv4Addr) -> bool {
    !address.is_unspecified() && !address.is_multicast() && !address.is_broadcast()
}
fn expected_flags(code: u8) -> Option<u8> {
    match code {
        1 | 2 | 3 | 5 | 6 => Some(0x40),
        4 | 9 | 10 | 14 | 15 => Some(0x80),
        7 | 8 | 16 | 17 | 18 | 32 => Some(0xc0),
        _ => None,
    }
}
fn discard_error(code: u8) -> bool {
    matches!(code, 6 | 7 | 17 | 18)
}
fn malformed(result: &mut ReceivedUpdate<'_>, code: u8) {
    result.errors.insert(code);
    if !discard_error(code) {
        result.handling = Handling::TreatAsWithdraw;
    }
}
fn no_as_zero(path: AsPath<'_>) -> bool {
    path.segments()
        .all(|segment| segment.asns().all(|asn| asn != 0))
}
fn family(peer: Negotiated, afi: u16, safi: u8) -> bool {
    safi == 1 && (afi == 1 && peer.ipv4_unicast || afi == 2 && peer.ipv6_unicast)
}
fn mp_hop(bytes: &[u8]) -> Option<NextHop> {
    match bytes {
        [a, b, c, d] => {
            let address = Ipv4Addr::new(*a, *b, *c, *d);
            unicast(address).then_some(NextHop {
                address: address.into(),
                link_local: None,
            })
        }
        bytes if matches!(bytes.len(), 16 | 32) => {
            let address =
                Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[..16]).expect("checked next-hop width"));
            // RFC 2545 §3 的首地址必须非链路地址；无 zone 的 link-local-only 不可递归解析。
            if address.is_unspecified()
                || address.is_multicast()
                || address.is_unicast_link_local()
                || address.to_ipv4_mapped().is_some()
            {
                return None;
            }
            let link_local = if bytes.len() == 32 {
                let local = Ipv6Addr::from(
                    <[u8; 16]>::try_from(&bytes[16..]).expect("checked paired next-hop width"),
                );
                if !local.is_unicast_link_local() {
                    return None;
                }
                Some(local)
            } else {
                None
            };
            Some(NextHop {
                address: address.into(),
                link_local,
            })
        }
        _ => None,
    }
}

/// 仅由 Session 为同一已解码 frame 调用；raw 与 update 必须来自同一输入，通知/日志引用因此保持精确。
pub(super) fn capture<'a>(
    config: Config,
    peer: Negotiated,
    update: Update<'a>,
    raw: &'a [u8],
) -> Result<ReceivedUpdate<'a>, Notice<'a>> {
    let invalid_network = || Notice::simple(3, 10);
    let ipv4_path_ids = peer.receives_add_path(Family::Ipv4);
    let withdrawn = update
        .withdrawn_with_path_ids(ipv4_path_ids)
        .map_err(|_| invalid_network())?;
    let announced = update
        .announced_with_path_ids(ipv4_path_ids)
        .map_err(|_| invalid_network())?;
    let mut result = ReceivedUpdate {
        raw,
        update,
        sections: [None; 4],
        retained: Codes::default(),
        errors: Codes::default(),
        handling: Handling::Accept,
        framing_error: false,
        end_of_rib: None,
        path: None,
        aggregator: None,
    };
    if peer.ipv4_unicast {
        result.sections[0] = Some(Section::Withdraw(withdrawn));
    }
    let mut seen = Codes::default();
    let mut count = 0usize;
    let mut next_hop = None;
    let mut mp_announced = None;
    let mut mp_withdrawn = None;
    let mut base_path = None;
    let mut as4_path = None;
    let mut as4_aggregator = None;
    let width = if peer.four_octet_asn {
        AsnWidth::Four
    } else {
        AsnWidth::Two
    };
    let kind = config.peer_kind();
    let internal = kind == crate::PeerKind::Internal;
    for scanned in update.scan_wire_attributes() {
        let (attribute, wire) = match scanned {
            Ok(value) => value,
            Err(_) => {
                result.framing_error = true;
                result.handling = Handling::TreatAsWithdraw;
                break;
            }
        };
        count += 1;
        let code = attribute.code;
        if !seen.insert(code) {
            if matches!(code, 14 | 15) {
                return Err(Notice::simple(3, 1));
            }
            continue;
        }
        // 已明确要求丢弃的属性不再参加语义准入；不能把 OLD 辅助属性或 eBGP LOCAL_PREF 当成可用政策。
        if kind == crate::PeerKind::External && code == 5
            || kind != crate::PeerKind::Internal && matches!(code, 9 | 10)
        {
            continue;
        }
        if peer.four_octet_asn && matches!(code, 17 | 18) {
            result.errors.insert(code);
            continue;
        }
        let Some(expected) = expected_flags(code) else {
            if attribute.flags & 0x80 == 0 {
                return Err(Notice {
                    code: 3,
                    subcode: 2,
                    data: wire,
                });
            }
            if attribute.flags & 0x40 != 0 {
                result.retained.insert(code);
            }
            continue;
        };
        if attribute.flags & 0xc0 != expected || expected != 0xc0 && attribute.flags & 0x20 != 0 {
            if matches!(code, 14 | 15) {
                return Err(Notice {
                    code: 3,
                    subcode: 9,
                    data: wire,
                });
            }
            malformed(&mut result, code);
            continue;
        }
        let value = match attribute.decode(width) {
            Ok(value) => value,
            Err(_) if matches!(code, 14 | 15) => {
                return Err(Notice {
                    code: 3,
                    subcode: 9,
                    data: wire,
                });
            }
            Err(_) => {
                malformed(&mut result, code);
                continue;
            }
        };
        match value {
            Value::NextHop(address) => {
                if !unicast(address) {
                    malformed(&mut result, code);
                    continue;
                }
                next_hop = Some(NextHop {
                    address: address.into(),
                    link_local: None,
                });
            }
            Value::AsPath(path) => {
                if !no_as_zero(path)
                    || code == 17
                        && (attribute.data.len() < 6 || !attribute.data.len().is_multiple_of(2))
                {
                    malformed(&mut result, code);
                    continue;
                }
                if code == 17 {
                    if path.segments().any(|segment| {
                        matches!(
                            segment.kind,
                            SegmentKind::ConfederationSequence | SegmentKind::ConfederationSet
                        )
                    }) {
                        result.errors.insert(code);
                    }
                    as4_path = Some((path, attribute.flags & 0x20 != 0));
                } else {
                    let first = path.segments().next().map(|segment| segment.kind);
                    let confederated = path.segments().any(|segment| {
                        matches!(
                            segment.kind,
                            SegmentKind::ConfederationSequence | SegmentKind::ConfederationSet
                        )
                    });
                    if confederated
                        && (kind == crate::PeerKind::External
                            || config.confederation_peer.is_none())
                        || kind == crate::PeerKind::Confederation
                            && first != Some(SegmentKind::ConfederationSequence)
                    {
                        malformed(&mut result, code);
                        continue;
                    }
                    base_path = Some(path);
                }
            }
            Value::Aggregator { asn, identifier } => {
                if asn == 0 {
                    malformed(&mut result, code);
                    continue;
                }
                let aggregator = Aggregator {
                    asn,
                    identifier,
                    partial: attribute.flags & 0x20 != 0,
                };
                if code == 18 {
                    as4_aggregator = Some(aggregator);
                } else {
                    result.aggregator = Some(aggregator);
                }
            }
            Value::MpReach(mp) => {
                if !family(peer, mp.afi, mp.safi) {
                    continue;
                }
                let path_ids = if mp.afi == 1 {
                    ipv4_path_ids
                } else {
                    peer.receives_add_path(Family::Ipv6)
                };
                let nlri = mp.prefixes_with_path_ids(path_ids).map_err(|_| Notice {
                    code: 3,
                    subcode: 9,
                    data: wire,
                })?;
                let accepted_width = match (mp.afi, mp.next_hop.len()) {
                    (1, 4) | (2, 16 | 32) => true,
                    (1, 16 | 32) => peer.receive_ipv4_via_ipv6,
                    _ => false,
                };
                if !accepted_width {
                    return Err(Notice {
                        code: 3,
                        subcode: 9,
                        data: wire,
                    });
                }
                let hop = mp_hop(mp.next_hop);
                if hop.is_none() {
                    malformed(&mut result, code);
                }
                mp_announced = Some((nlri, hop));
            }
            Value::MpUnreach(mp) => {
                if !family(peer, mp.afi, mp.safi) {
                    continue;
                }
                let path_ids = if mp.afi == 1 {
                    ipv4_path_ids
                } else {
                    peer.receives_add_path(Family::Ipv6)
                };
                mp_withdrawn = Some(mp.prefixes_with_path_ids(path_ids).map_err(|_| Notice {
                    code: 3,
                    subcode: 9,
                    data: wire,
                })?);
            }
            _ => {}
        }
        result.retained.insert(code);
    }
    // OLD 聚合器非 AS_TRANS 时，其重新聚合已使辅助信息失效；两种辅助属性必须一起忽略。
    if let (Some(base), Some(mut aggregator)) = (result.aggregator, as4_aggregator) {
        if base.asn == 23456 {
            aggregator.partial |= base.partial;
            result.aggregator = Some(aggregator);
        } else {
            as4_path = None;
        }
    }
    result.path = base_path.map(|base| Path::reconstruct(base, as4_path));
    let base_announced = peer.ipv4_unicast && !announced.bytes().is_empty();
    let has_reachable =
        base_announced || mp_announced.is_some_and(|(nlri, _)| !nlri.bytes().is_empty());
    if has_reachable {
        for code in [1, 2] {
            if !result.retained.contains(code) {
                malformed(&mut result, code);
            }
        }
        if base_announced && next_hop.is_none() {
            malformed(&mut result, 3);
        }
        if internal && !result.retained.contains(5) {
            malformed(&mut result, 5);
        }
    }
    // 有属性错误却没有可辨认的 reachable NLRI，不能把“缺少 NLRI”当作无事发生或只保留旧路由。
    if result.handling == Handling::TreatAsWithdraw && !has_reachable {
        return Err(Notice::simple(3, 1));
    }
    result.sections[1] = mp_withdrawn.map(Section::Withdraw);
    if base_announced {
        result.sections[2] = Some(Section::Announce(announced, next_hop));
    }
    result.sections[3] = mp_announced.map(|(nlri, hop)| Section::Announce(nlri, hop));
    if !result.framing_error && withdrawn.bytes().is_empty() && announced.bytes().is_empty() {
        if count == 0 && peer.ipv4_unicast {
            result.end_of_rib = Some(Family::Ipv4);
        } else if count == 1 {
            result.end_of_rib = mp_withdrawn
                .filter(|nlri| nlri.bytes().is_empty())
                .map(|nlri| nlri.family());
        }
    }
    Ok(result)
}
