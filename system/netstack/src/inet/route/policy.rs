//! 同代选表规则按流量来源分段；报文只扫描所在方向，查询按优先级合并。

use alloc::{sync::Arc, vec::Vec};
use core::net::IpAddr;
use network_protocol::SocketError;
use router_protocol::policy_routing::{Rule, Rules};

struct RuleSet {
    entries: Vec<Rule>,
    local_count: usize,
    generation: u64,
}

/// 同一生效规则的共享只读根，配置和接收队列借用同一份优先级顺序。
#[derive(Clone, Default)]
pub(in crate::inet) struct SelectionRules(Option<Arc<RuleSet>>);

pub(super) fn matches_source(rule: &Rule, source: IpAddr, ingress: u32, mark: u32) -> bool {
    (!source.is_unspecified() || rule.prefix_length == 0) && rule.matches(source, ingress, mark)
}

impl SelectionRules {
    /// 捕获已校验规则；每条规则只存一次，各段保留原优先级。
    pub(in crate::inet) fn capture(rules: Rules<'_>) -> Result<Self, SocketError> {
        let count = rules.iter().len();
        if count == 0 {
            return Ok(Self::default());
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(count)
            .map_err(|_| SocketError::NoMemory)?;
        entries.extend(rules.iter().filter(|rule| rule.is_local()));
        let local_count = entries.len();
        entries.extend(rules.iter().filter(|rule| !rule.is_local()));
        Ok(Self(Some(
            Arc::try_new(RuleSet {
                entries,
                local_count,
                generation: crate::identity::next(),
            })
            .map_err(|_| SocketError::NoMemory)?,
        )))
    }

    pub(in crate::inet) fn local(&self) -> &[Rule] {
        self.0
            .as_ref()
            .map_or(&[], |rules| &rules.entries[..rules.local_count])
    }

    pub(in crate::inet) fn transit(&self) -> &[Rule] {
        self.0
            .as_ref()
            .map_or(&[], |rules| &rules.entries[rules.local_count..])
    }

    /// 同一进程内不复用的规则版本。没有转发规则时返回零。
    pub(in crate::inet) fn transit_generation(&self) -> u64 {
        self.0.as_ref().map_or(0, |rules| {
            if rules.local_count == rules.entries.len() {
                0
            } else {
                rules.generation
            }
        })
    }

    /// 为原始源地址、入口和标记记录首个匹配规则的位置，或所有规则均已检查。
    pub(in crate::inet) fn prepare_transit(
        &self,
        source: IpAddr,
        ingress: u32,
        mark: u32,
    ) -> [u64; 2] {
        let rules = self.transit();
        if rules.is_empty() {
            return [0; 2];
        }
        let first = rules
            .iter()
            .position(|rule| matches_source(rule, source, ingress, mark))
            .unwrap_or(rules.len());
        if first == 0 {
            return [0; 2];
        }
        // 已验证的优先级严格升序且小于 32766，位置可与接口号共用一个字。
        [
            self.transit_generation(),
            (u64::from(ingress) << 32) | first as u64,
        ]
    }

    /// 同一原帧、同一流量标记才能调用。版本或入口变化时从当前规则的首项开始。
    pub(in crate::inet) fn transit_from(
        &self,
        ingress: u32,
        [generation, start]: [u64; 2],
    ) -> &[Rule] {
        let rules = self.transit();
        if generation != 0
            && generation == self.transit_generation()
            && start >> 32 == u64::from(ingress)
        {
            rules.get(start as u32 as usize..).unwrap_or(rules)
        } else {
            rules
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use router_protocol::{configuration::Configuration, policy_routing::Tables};

    #[test]
    fn transit_hint_never_skips_new_rules_after_replacement_or_ingress_change() {
        let build = |priorities: (u32, u32)| {
            let input = alloc::format!(
                r#"{{"version":1,"interfaces":{{"port":{{"device":{{"kind":"name","value":"eth0"}}}}}},
                    "policy_routing":{{"tables":[{{"id":100}},{{"id":200}}],
                    "rules":[{{"priority":{},"source":"0.0.0.0","prefix_length":0,"table":100}},
                    {{"priority":{},"source":"::","prefix_length":0,"ingress":"port","mark":42,"table":200}}]}}}}"#,
                priorities.0,
                priorities.1,
            );
            let config = Configuration::decode(input.as_bytes()).unwrap();
            let bytes = config.policy_source(|_| Ok(Some(2))).unwrap();
            SelectionRules::capture(Tables::parse(&bytes).unwrap().rules()).unwrap()
        };
        let source = "2001:db8::9".parse().unwrap();
        let old = build((10, 20));
        let prepared = old.prepare_transit(source, 2, 42);
        assert_ne!(prepared[0], 0);
        assert_eq!(old.transit_from(2, prepared), &old.transit()[1..]);
        assert_eq!(old.transit_from(3, prepared), old.transit());
        assert!(old.transit_from(2, old.prepare_transit(source, 2, 0)).is_empty());

        let new = build((20, 10));
        assert_eq!(new.transit_from(2, prepared), new.transit());
        assert_eq!(new.transit()[0].table, 200);
    }
}
