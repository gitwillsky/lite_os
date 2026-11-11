//! 同代选表规则按流量来源分段；报文只扫描所在方向，查询按优先级合并。

use alloc::vec::Vec;
use network_protocol::SocketError;
use router_protocol::policy_routing::{Rule, Rules};

#[derive(Default)]
pub(in crate::inet) struct SelectionRules {
    entries: Vec<Rule>,
    local_count: usize,
}

impl SelectionRules {
    /// 捕获已校验规则；每条规则只存一次，各段保留原优先级。
    pub(in crate::inet) fn capture(rules: Rules<'_>) -> Result<Self, SocketError> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(rules.iter().len())
            .map_err(|_| SocketError::NoMemory)?;
        entries.extend(rules.iter().filter(|rule| rule.is_local()));
        let local_count = entries.len();
        entries.extend(rules.iter().filter(|rule| !rule.is_local()));
        Ok(Self {
            entries,
            local_count,
        })
    }

    pub(in crate::inet) fn local(&self) -> &[Rule] {
        &self.entries[..self.local_count]
    }

    pub(in crate::inet) fn transit(&self) -> &[Rule] {
        &self.entries[self.local_count..]
    }

    /// 按原优先级顺序借用当前生效的两段。
    pub(in crate::inet) fn ordered(&self) -> impl Iterator<Item = Rule> + Clone + '_ {
        let mut local = self.local().iter().copied().peekable();
        let mut transit = self.transit().iter().copied().peekable();
        core::iter::from_fn(move || {
            if local.peek().is_some_and(|left| {
                transit
                    .peek()
                    .is_none_or(|right| left.priority < right.priority)
            }) {
                local.next()
            } else {
                transit.next()
            }
        })
    }
}
