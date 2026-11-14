//! IGMP/MLD 共用的 ASM 成员状态机；协议编码和查询器兼容状态各归对应接口。
use super::{SendTime, random_delay};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use packet_wire::multicast::{RecordType, SourceAddresses};
// RFC 9776/9777 允许限制查询来源存储；仅实际 GSS 查询时分配，超额/OOM 保留旧义务。
const SOURCE_LIMIT: usize = 128;
/// 保持两族地址各自的紧凑布局，不在每个来源存储带族标签的宽地址。
pub(in crate::inet) trait GroupAddress: Copy + Ord {
    /// 同族来源返回地址，其他族返回 None。
    fn from_ip(address: IpAddr) -> Option<Self>;
}
impl GroupAddress for Ipv4Addr {
    fn from_ip(address: IpAddr) -> Option<Self> {
        if let IpAddr::V4(ip) = address {
            Some(ip)
        } else {
            None
        }
    }
}
impl GroupAddress for Ipv6Addr {
    fn from_ip(address: IpAddr) -> Option<Self> {
        if let IpAddr::V6(ip) = address {
            Some(ip)
        } else {
            None
        }
    }
}
/// 当前接口选择的报告语义；IGMPv1 不发送 Leave，其余旧代只由最后报告者退组。
#[derive(Clone, Copy)]
pub(in crate::inet) enum Mode {
    Current,
    Legacy { leave: bool },
}
/// 已提交给协议 codec 的借用状态，不持有第二份成员或来源集合。
pub(in crate::inet) enum Report<'a, A> {
    Legacy {
        group: A,
        leave: bool,
    },
    Current {
        group: A,
        kind: RecordType,
        sources: &'a [A],
    },
}
enum Membership {
    Off,
    Changing {
        leave: bool,
        sent: u8,
        total: u8,
        at: SendTime,
    },
    Joined,
}
struct Specific<A> {
    at: SendTime,
    // None 是完整组状态；Some 是经过合并的来源查询，发送后按前缀消费，不能永久保存来源。
    sources: Option<Vec<A>>,
    first: usize,
}
pub(in crate::inet) struct Listener<A: GroupAddress> {
    // OWNER: 接口对 socket 成员的聚合引用；唯一 socket 表在同一 NET guard 内成对增减。
    // 与地址所需组的引用取并集，不能因地址撤销或单个 socket 退出而提前退组。
    pub(in crate::inet) users: usize,
    group: A,
    membership: Membership,
    general: Option<SendTime>,
    specific: Option<Specific<A>>,
    last_reporter: bool,
}
impl<A: GroupAddress> Listener<A> {
    /// 根据同组地址的当前引用收敛监听义务；共享 solicited-node 组不得因单个地址退出而退组。
    pub(in crate::inet) fn synchronize(
        &mut self,
        wanted: bool,
        start: u64,
        now: u64,
        robustness: u8,
    ) {
        if wanted
            && matches!(
                self.membership,
                Membership::Off | Membership::Changing { leave: true, .. }
            )
        {
            self.join(start, robustness);
        } else if !wanted
            && matches!(
                self.membership,
                Membership::Joined | Membership::Changing { leave: false, .. }
            )
        {
            self.leave(now, robustness);
        }
    }
    pub(in crate::inet) fn inactive(&self) -> bool {
        matches!(self.membership, Membership::Off)
    }
    pub(in crate::inet) fn new(group: A) -> Self {
        Self {
            users: 0,
            group,
            membership: Membership::Off,
            general: None,
            specific: None,
            last_reporter: false,
        }
    }
    /// RFC 3590：未指定源的初始报告不能替代地址可用后的正式报告。
    pub(in crate::inet) fn address_ready(&mut self, now: u64) {
        self.general = Some(SendTime::At(now));
    }
    pub(in crate::inet) fn group(&self) -> A {
        self.group
    }
    pub(in crate::inet) fn reset(&mut self) {
        let users = self.users;
        *self = Self::new(self.group);
        self.users = users;
    }
    /// 接收资格立即随意图变化，不等待报告提交；退组重传期间不能继续接收应用数据。
    pub(in crate::inet) fn listening(&self) -> bool {
        matches!(
            self.membership,
            Membership::Joined | Membership::Changing { leave: false, .. }
        )
    }
    /// 回环或协议规定免报告的组只收敛本地资格；不能留下永远到期的发送义务。
    pub(in crate::inet) fn quiet(&mut self) {
        if let Membership::Changing { leave, .. } = self.membership {
            self.membership = if leave {
                Membership::Off
            } else {
                Membership::Joined
            };
        }
        self.general = None;
        self.specific = None;
    }
    pub(in crate::inet) fn join(&mut self, now: u64, robustness: u8) {
        self.membership = Membership::Changing {
            leave: false,
            sent: 0,
            total: robustness,
            at: SendTime::At(now),
        };
    }
    pub(in crate::inet) fn ready(&self) -> bool {
        matches!(
            self.membership,
            Membership::Joined
                | Membership::Changing {
                    leave: false,
                    sent: 1..,
                    ..
                }
        )
    }
    pub(in crate::inet) fn leave(&mut self, now: u64, robustness: u8) {
        self.membership = if self.ready() {
            Membership::Changing {
                leave: true,
                sent: 0,
                total: robustness,
                at: SendTime::At(now),
            }
        } else {
            Membership::Off
        };
        self.general = None;
        self.specific = None;
    }
    pub(in crate::inet) fn next_deadline(&self) -> Option<u64> {
        let change = match self.membership {
            Membership::Changing { at, .. } => at.deadline(),
            _ => None,
        };
        [
            change,
            self.general.and_then(SendTime::deadline),
            self.specific
                .as_ref()
                .and_then(|response| response.at.deadline()),
        ]
        .into_iter()
        .flatten()
        .min()
    }
    /// 接收已经由协议校验的查询；None 为全组查询，来源仅用于本次报告合并。
    pub(in crate::inet) fn query(
        &mut self,
        group: Option<A>,
        maximum: u64,
        sources: Option<SourceAddresses<'_>>,
        now: u64,
    ) {
        if !self.listening() || group.is_some_and(|group| group != self.group) {
            return;
        }
        let at = now.saturating_add(random_delay(maximum));
        if self
            .general
            .is_some_and(|pending| pending.deadline().is_none_or(|pending| pending <= at))
        {
            return;
        }
        if group.is_none() {
            self.general = Some(SendTime::At(at));
            return;
        }
        let sources = sources.filter(|sources| sources.len() != 0);
        let previous = self.specific.as_ref();
        let next = previous
            .and_then(|reply| reply.at.deadline())
            .map_or(at, |old| old.min(at));
        if sources.is_none() || previous.is_some_and(|response| response.sources.is_none()) {
            self.specific = Some(Specific {
                at: SendTime::At(next),
                sources: None,
                first: 0,
            });
        } else if let Some(sources) = sources {
            let old = previous
                .and_then(|response| {
                    response
                        .sources
                        .as_deref()
                        .map(|sources| &sources[response.first..])
                })
                .unwrap_or(&[]);
            let Some(merged) = merge_sources(old, sources) else {
                return;
            };
            self.specific = Some(Specific {
                at: SendTime::At(next),
                sources: Some(merged),
                first: 0,
            });
        }
    }
    /// 旧代同组报告抑制本机回复；现代协议不因其他主机报告而清除自身义务。
    pub(in crate::inet) fn report(&mut self, group: A, legacy: bool) {
        if self.listening() && group == self.group && legacy {
            self.membership = Membership::Joined;
            self.general = None;
            self.specific = None;
            self.last_reporter = false;
        }
    }
    pub(in crate::inet) fn poll(
        &mut self,
        now: u64,
        source_budget: usize,
        mode: Mode,
        send: &mut impl FnMut(Report<'_, A>) -> bool,
    ) {
        let legacy = !matches!(mode, Mode::Current);
        if let Membership::Changing {
            leave,
            sent,
            total,
            at,
        } = &mut self.membership
            && at.ready(now)
        {
            if !(*leave
                && legacy
                && (!self.last_reporter || matches!(mode, Mode::Legacy { leave: false })))
            {
                let packet = if legacy {
                    Report::Legacy {
                        group: self.group,
                        leave: *leave,
                    }
                } else {
                    Report::Current {
                        group: self.group,
                        kind: if *leave {
                            RecordType::ChangeToInclude
                        } else {
                            RecordType::ChangeToExclude
                        },
                        sources: &[],
                    }
                };
                if !send(packet) {
                    *at = SendTime::Writable;
                    return;
                }
                self.last_reporter = legacy && !*leave;
                // 旧代的同一份 Report 已覆盖当前状态和待回复查询，不重复发送等价报文。
                if legacy && !*leave {
                    self.general = None;
                    self.specific = None;
                }
                *sent += 1;
            } else {
                *sent = *total;
            }
            if *sent == *total || legacy && *leave {
                self.membership = if *leave {
                    Membership::Off
                } else {
                    Membership::Joined
                };
            } else {
                *at = SendTime::At(now.saturating_add(random_delay(if legacy {
                    10_000
                } else {
                    1000
                })));
            }
        }
        if !self.ready() {
            return;
        }
        if self.general.is_some_and(|at| at.ready(now)) {
            let packet = if legacy {
                Report::Legacy {
                    group: self.group,
                    leave: false,
                }
            } else {
                Report::Current {
                    group: self.group,
                    kind: RecordType::ModeIsExclude,
                    sources: &[],
                }
            };
            if !send(packet) {
                self.general = Some(SendTime::Writable);
                return;
            }
            self.last_reporter = legacy;
            self.general = None;
            self.specific = None;
        }
        // 来源上限让最多两个标准 MTU 报告即可完成；未提交的尾部保留，不能因背压丢失来源。
        for _ in 0..2 {
            let Some(response) = &mut self.specific else {
                break;
            };
            if !response.at.ready(now) {
                break;
            }
            let sources = response.sources.as_deref().unwrap_or(&[]);
            let count = source_budget.min(sources.len() - response.first);
            let packet = if legacy {
                Report::Legacy {
                    group: self.group,
                    leave: false,
                }
            } else {
                Report::Current {
                    group: self.group,
                    kind: if response.sources.is_some() {
                        RecordType::ModeIsInclude
                    } else {
                        RecordType::ModeIsExclude
                    },
                    sources: &sources[response.first..response.first + count],
                }
            };
            if !send(packet) {
                response.at = SendTime::Writable;
                return;
            }
            self.last_reporter = legacy;
            response.first += count;
            if legacy || response.first == sources.len() {
                self.specific = None;
            } else {
                response.at = SendTime::At(now);
            }
        }
    }
}
fn merge_sources<A: GroupAddress>(previous: &[A], sources: SourceAddresses<'_>) -> Option<Vec<A>> {
    let mut merged = Vec::new();
    merged
        .try_reserve_exact((previous.len() + sources.len()).min(SOURCE_LIMIT))
        .ok()?;
    merged.extend_from_slice(previous);
    for address in sources {
        let address = A::from_ip(address)?;
        if let Err(at) = merged.binary_search(&address) {
            if merged.len() == SOURCE_LIMIT {
                return None;
            }
            merged.insert(at, address);
        }
    }
    Some(merged)
}
