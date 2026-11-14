//! MLD 查询器参数与协议消息适配；成员状态机由两族共用，接口只保留一份兼容时钟。
use core::net::Ipv6Addr;
use packet_wire::{
    mld::MldMessage,
    multicast::{decode_code8, decode_code16},
};
pub(super) type Listener = crate::inet::multicast::listener::Listener<Ipv6Addr>;
/// RFC 9777 的接口级查询参数与旧版本查询器寿命；各组只借用，不复制时钟。
pub(super) struct Parameters {
    older_until: Option<u64>,
    robustness: u8,
    query_interval: u64,
}
impl Parameters {
    pub(super) fn new() -> Self {
        Self {
            older_until: None,
            robustness: 2,
            query_interval: 125_000,
        }
    }
    pub(super) fn robustness(&self) -> u8 {
        self.robustness
    }
    pub(super) fn v1(&self, now: u64) -> bool {
        self.older_until.is_some_and(|at| at > now)
    }
    pub(super) fn next_deadline(&self) -> Option<u64> {
        self.older_until
    }
    pub(super) fn expire(&mut self, now: u64) -> bool {
        if self.next_deadline().is_some_and(|at| at <= now) {
            self.older_until = None;
            true
        } else {
            false
        }
    }
    /// 完整校验查询后更新接口参数；组专属查询同样影响接口的查询器兼容模式。
    pub(super) fn receive(&mut self, message: MldMessage<'_>, now: u64) -> bool {
        let MldMessage::Query {
            maximum_response_code,
            group,
            source_query,
        } = message
        else {
            return true;
        };
        if !group.is_unspecified() && !group.is_multicast()
            || group.is_unspecified() && source_query.is_some_and(|query| query.sources.len() != 0)
            || source_query.is_some_and(|mut query| {
                query
                    .sources
                    .any(|address| address.is_unspecified() || address.is_multicast())
            })
        {
            return false;
        }
        if let Some(query) = source_query {
            self.robustness = if query.robustness == 0 {
                2
            } else {
                query.robustness
            };
            self.query_interval = if query.query_interval_code == 0 {
                125_000
            } else {
                u64::from(decode_code8(query.query_interval_code)) * 1000
            };
        } else {
            self.older_until = Some(now.saturating_add(
                u64::from(self.robustness) * self.query_interval + u64::from(maximum_response_code),
            ));
        }
        true
    }
}

pub(super) fn receive(listener: &mut Listener, message: MldMessage<'_>, now: u64, legacy: bool) {
    match message {
        MldMessage::Query {
            maximum_response_code,
            group,
            source_query,
        } => {
            let maximum = if source_query.is_some() {
                u64::from(decode_code16(maximum_response_code))
            } else {
                u64::from(maximum_response_code)
            };
            listener.query(
                (!group.is_unspecified()).then_some(group),
                maximum,
                source_query.map(|query| query.sources),
                now,
            );
        }
        MldMessage::ReportV1 { done: false, group } => listener.report(group, legacy),
        _ => {}
    }
}
