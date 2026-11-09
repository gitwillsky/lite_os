//! 不可变路由准入编译；按来源标记分桶、前缀索引预筛选，最终仍按原规则序号 first-match。
use crate::{Address, Attributes, Error, NextHop, Poptrie, Route, Source};
use alloc::{sync::Arc, vec::Vec};
use core::net::IpAddr;
use fallible_tree::FallibleMap;
use router_protocol::policy::{Action, Disposition, Policy};
type Ipv4Source<M> = Arc<Source<u32, (), NextHop, M>>;

#[derive(Clone, Copy)]
struct Match {
    id: u32,
    minimum: u8,
    maximum: u8,
}

struct Index<A> {
    prefixes: Poptrie<A>,
    order: Vec<Match>,
}

impl<A: Address> Index<A> {
    fn build(
        policy: &Policy,
        rows: &[(u16, u32)],
        zero: A,
        convert: fn(IpAddr) -> Option<A>,
    ) -> Result<Self, Error> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(rows.len())
            .map_err(|_| Error::NoMemory)?;
        for (_, id) in rows {
            let key = match &policy.rules[*id as usize].prefix {
                None => Some((zero, 0, 0, A::BITS)),
                Some(prefix) => convert(prefix.network).map(|network| {
                    let (minimum, maximum) = prefix.lengths().expect("compiled prefix validated");
                    (network, prefix.length, minimum, maximum)
                }),
            };
            if let Some((network, length, minimum, maximum)) = key {
                entries.push((
                    network,
                    length,
                    Match {
                        id: *id,
                        minimum,
                        maximum,
                    },
                ));
            }
        }
        entries.sort_unstable_by_key(|(network, length, row)| (*network, *length, row.id));
        // 同桶/同前缀没有其他条件：被更早规则覆盖的全部长度永不获胜。每条保留规则至少增加一个长度，最多 BITS+1 条。
        let mut previous = None;
        let mut covered = [0u64; 3];
        entries.retain(|(network, length, row)| {
            if previous != Some((*network, *length)) {
                previous = Some((*network, *length));
                covered = [0; 3];
            }
            let mut added = false;
            for length in row.minimum..=row.maximum {
                let bit = 1u64 << (length % 64);
                let word = &mut covered[usize::from(length / 64)];
                added |= *word & bit == 0;
                *word |= bit;
            }
            added
        });
        let prefixes = Poptrie::build(
            entries
                .iter()
                .map(|(network, length, _)| (*network, *length)),
        )?;
        let mut order = Vec::new();
        order
            .try_reserve_exact(entries.len())
            .map_err(|_| Error::NoMemory)?;
        order.extend(entries.into_iter().map(|(_, _, id)| id));
        Ok(Self { prefixes, order })
    }
    fn matching(&self, network: A, length: u8, mut best: u32) -> u32 {
        for prefix in self.prefixes.matching(network) {
            for row in &self.order[prefix.range()] {
                if row.id >= best {
                    break;
                }
                if row.minimum <= length && length <= row.maximum {
                    best = row.id;
                    break;
                }
            }
            if best == 0 {
                break;
            }
        }
        best
    }
}
struct Bucket {
    code: u16,
    ipv4: Index<u32>,
    ipv6: Index<u128>,
}
/// RIB 唯一的已编译策略；只保留动作和索引，不保留第二份持久配置树，原始声明始终归 Sources。
pub(super) struct Compiled {
    default: Disposition,
    actions: Vec<Action>,
    buckets: Vec<Bucket>,
}
impl Compiled {
    pub(super) fn capture(policy: Policy) -> Result<Option<Arc<Self>>, Error> {
        // 1. 冻结策略前验证完整规则；无约束策略不需要分配程序对象。
        policy.validate().map_err(|_| Error::InvalidSource)?;
        u32::try_from(policy.rules.len()).map_err(|_| Error::Capacity)?;
        if policy.rules.is_empty() && policy.default == Disposition::Permit {
            return Ok(None);
        }
        // 2. 按可信标记分桶，再按前缀建立候选；原规则序号始终保留 first-match 优先级。
        let mut rows = Vec::new();
        rows.try_reserve_exact(policy.rules.len())
            .map_err(|_| Error::NoMemory)?;
        rows.extend(policy.rules.iter().enumerate().map(|(id, rule)| {
            (
                rule.protocol.map_or(0, |protocol| u16::from(protocol) + 1),
                id as u32,
            )
        }));
        rows.sort_unstable();
        let mut buckets = Vec::new();
        buckets
            .try_reserve_exact(rows.chunk_by(|left, right| left.0 == right.0).count())
            .map_err(|_| Error::NoMemory)?;
        for rows in rows.chunk_by(|left, right| left.0 == right.0) {
            buckets.push(Bucket {
                code: rows[0].0,
                ipv4: Index::build(&policy, rows, 0u32, |address| match address {
                    IpAddr::V4(address) => Some(address.into()),
                    _ => None,
                })?,
                ipv6: Index::build(&policy, rows, 0u128, |address| match address {
                    IpAddr::V6(address) => Some(address.into()),
                    _ => None,
                })?,
            });
        }
        // 3. 索引完成后退休输入条件，只保留执行动作；完整持久模型仍由配置 owner 拥有。
        let mut actions = Vec::new();
        actions
            .try_reserve_exact(policy.rules.len())
            .map_err(|_| Error::NoMemory)?;
        actions.extend(policy.rules.into_iter().map(|rule| rule.action));
        Arc::try_new(Self {
            default: policy.default,
            actions,
            buckets,
        })
        .map(Some)
        .map_err(|_| Error::NoMemory)
    }
    fn bucket(&self, code: u16) -> Option<&Bucket> {
        self.buckets
            .binary_search_by_key(&code, |bucket| bucket.code)
            .ok()
            .map(|index| &self.buckets[index])
    }
    fn decision(&self, buckets: [Option<&Bucket>; 2], network: IpAddr, length: u8) -> Action {
        let mut best = u32::MAX;
        for bucket in buckets.into_iter().flatten() {
            best = match network {
                IpAddr::V4(network) => bucket.ipv4.matching(network.into(), length, best),
                IpAddr::V6(network) => bucket.ipv6.matching(network.into(), length, best),
            };
        }
        self.actions.get(best as usize).map_or_else(
            || match self.default {
                Disposition::Permit => Action::Permit {
                    preference: None,
                    metric: None,
                },
                Disposition::Deny => Action::Deny,
            },
            |action| *action,
        )
    }
    pub(super) fn apply<M>(
        &self,
        source: &Ipv4Source<M>,
        protocol: u8,
    ) -> Result<Option<Ipv4Source<M>>, Error> {
        let buckets = [self.bucket(0), self.bucket(u16::from(protocol) + 1)];
        if buckets
            .iter()
            .flatten()
            .all(|bucket| bucket.ipv4.order.is_empty())
        {
            return Ok((self.default == Disposition::Permit).then(|| source.clone()));
        }
        let mut routes = Vec::new();
        let mut attributes = Vec::new();
        let mut by_value = FallibleMap::new();
        for route in source.routes() {
            let Action::Permit { preference, metric } =
                self.decision(buckets, IpAddr::V4(route.network.into()), route.length)
            else {
                continue;
            };
            let old = &source.attributes()[route.attributes as usize];
            let key = (
                preference.unwrap_or(old.preference),
                metric.unwrap_or(old.metric),
            );
            let attribute = if let Some(id) = by_value.get(&key) {
                *id
            } else {
                let id = u32::try_from(attributes.len()).map_err(|_| Error::Capacity)?;
                attributes.try_reserve(1).map_err(|_| Error::NoMemory)?;
                by_value.try_insert(key, id).map_err(|_| Error::NoMemory)?;
                attributes.push(Attributes {
                    preference: key.0,
                    metric: key.1,
                    data: (),
                });
                id
            };
            routes.try_reserve(1).map_err(|_| Error::NoMemory)?;
            routes.push(Route {
                attributes: attribute,
                ..*route
            });
        }
        if routes.is_empty() {
            return Ok(None);
        }
        source.project_attributes(attributes, routes).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use router_protocol::policy::{Prefix, Rule};

    #[test]
    fn compiled_first_match_agrees_with_linear_dual_stack_rules_and_bounds_shadowed_rows() {
        let mut rules = Vec::new();
        for id in 0..1024u32 {
            let (network, length) = if id % 2 == 0 {
                (IpAddr::V4((id << 24).into()), 8)
            } else {
                (IpAddr::V6((u128::from(id) << 112).into()), 16)
            };
            rules.push(Rule {
                protocol: (id % 3 != 0).then_some((id % 7) as u8),
                prefix: Some(Prefix {
                    network,
                    length,
                    minimum: Some(length),
                    maximum: None,
                }),
                action: if id % 5 == 0 {
                    Action::Deny
                } else {
                    Action::Permit {
                        preference: Some(id + 1),
                        metric: Some(id),
                    }
                },
            });
        }
        let policy = Policy {
            default: Disposition::Deny,
            rules,
        };
        let compiled = Compiled::capture(policy.clone()).unwrap().unwrap();
        for number in 0..4096u32 {
            let protocol = (number % 9) as u8;
            let (network, length) = if number % 2 == 0 {
                (IpAddr::V4((number << 24 | (number >> 8) << 16).into()), 16)
            } else {
                (
                    IpAddr::V6((u128::from(number) << 112 | u128::from(number) << 96).into()),
                    64,
                )
            };
            let expected = policy
                .rules
                .iter()
                .find(|rule| {
                    rule.protocol.is_none_or(|value| value == protocol)
                        && rule.prefix.as_ref().is_none_or(|prefix| {
                            let (minimum, maximum) = prefix.lengths().unwrap();
                            let contains = match (prefix.network, network) {
                                (IpAddr::V4(base), IpAddr::V4(address)) => {
                                    u32::from(base)
                                        == u32::from(address)
                                            & u32::MAX
                                                .checked_shl(32 - u32::from(prefix.length))
                                                .unwrap_or(0)
                                }
                                (IpAddr::V6(base), IpAddr::V6(address)) => {
                                    u128::from(base)
                                        == u128::from(address)
                                            & u128::MAX
                                                .checked_shl(128 - u32::from(prefix.length))
                                                .unwrap_or(0)
                                }
                                _ => false,
                            };
                            contains && minimum <= length && length <= maximum
                        })
                })
                .map_or(Action::Deny, |rule| rule.action);
            assert_eq!(
                compiled.decision(
                    [compiled.bucket(0), compiled.bucket(u16::from(protocol) + 1)],
                    network,
                    length
                ),
                expected
            );
        }
        let broad = Rule {
            protocol: None,
            prefix: None,
            action: Action::Deny,
        };
        let mut policy = Policy {
            default: Disposition::Permit,
            rules: alloc::vec![broad; 1024],
        };
        policy.rules[1].prefix = Some(Prefix {
            network: "192.0.2.0".parse().unwrap(),
            length: 24,
            minimum: None,
            maximum: Some(32),
        });
        policy.rules[1].action = Action::Permit {
            preference: None,
            metric: None,
        };
        let compiled = Compiled::capture(policy).unwrap().unwrap();
        assert_eq!(
            compiled.bucket(0).unwrap().ipv4.order.len(),
            2,
            "same-prefix shadowed rules cannot grow the query walk"
        );
        assert_eq!(
            compiled.decision([compiled.bucket(0), None], "192.0.2.1".parse().unwrap(), 32),
            Action::Deny,
            "first broad rule wins before a narrower permit"
        );
    }
}
