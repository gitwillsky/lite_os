//! 规则页按配置代际与链内位置识别声明，计数页只在对应的执行版本上关联。

use crate::routes::{RuleKind, interface_names, rule_address};
use network_config::{
    ConfigurationStatus, ManagementClient, Revision,
    configuration::{
        ConfigurationName, Firewall, PreroutingRule, RouterConfiguration, connections::Family,
    },
    firewall::counters::{Cursor, LIMIT},
};
use std::{io, sync::Arc};

#[derive(Clone)]
pub(super) struct Key {
    pub(super) chain: Option<ConfigurationName>,
    pub(super) index: usize,
}

pub(super) struct Entry {
    pub(super) key: Key,
    pub(super) rule: PreroutingRule,
    offset: usize,
}

pub(super) struct Catalog {
    pub(super) revision: Revision,
    pub(super) rows: Vec<Entry>,
    pub(super) interfaces: Vec<ConfigurationName>,
    pub(super) chains: Vec<ConfigurationName>,
    pub(super) sets: Vec<ConfigurationName>,
}

#[derive(Clone, Copy)]
pub(super) struct Scope {
    pub(super) family: Family,
    pub(super) kind: RuleKind,
}

pub(super) enum Change {
    Add(PreroutingRule),
    Put(Key, PreroutingRule),
    Enable(Key, bool),
    Remove(Key),
}

pub(super) type Counts = Vec<Option<(u64, u64)>>;

impl Catalog {
    fn read(client: &mut ManagementClient, domain: Option<&str>, scope: Scope) -> io::Result<Self> {
        let (revision, config) = read_configuration(client)?;
        let domain = domain
            .map(|name| {
                config
                    .routing_domains
                    .get_key_value(name)
                    .map(|(key, _)| key)
                    .ok_or_else(missing)
            })
            .transpose()?;
        let firewall = domain.map_or(&config.firewall, |name| {
            &config.routing_domains[name].firewall
        });
        let mut offset = 0;
        let mut rows = Vec::new();
        for (chain, rules) in std::iter::once((None, &firewall.prerouting)).chain(
            firewall
                .chains
                .iter()
                .map(|(name, rules)| (Some(name), rules)),
        ) {
            for (index, rule) in rules.iter().enumerate() {
                if scope.kind.includes(&rule.action)
                    && rule_address(rule).is_none_or(|address| scope.family.includes(address))
                {
                    rows.push(Entry {
                        key: Key {
                            chain: chain.cloned(),
                            index,
                        },
                        rule: rule.clone(),
                        offset,
                    });
                }
                offset += 1;
            }
        }
        Ok(Self {
            revision,
            rows,
            interfaces: interface_names(&config, domain),
            chains: firewall.chains.keys().cloned().collect(),
            sets: firewall.address_sets.keys().cloned().collect(),
        })
    }

    pub(super) fn sample(
        domain: Option<&str>,
        scope: Scope,
        previous: Option<Arc<Self>>,
        start: usize,
    ) -> io::Result<(Arc<Self>, usize, Counts)> {
        let mut client = ManagementClient::local();
        let settings = client.snapshot()?;
        let (catalog, start) = if let Some(previous) =
            previous.filter(|previous| previous.revision == settings.generation)
        {
            (previous, start)
        } else {
            (Arc::new(Self::read(&mut client, domain, scope)?), 0)
        };
        let rows = &catalog.rows[start..catalog.rows.len().min(start + LIMIT)];
        let mut counts = vec![None; rows.len()];
        if settings.configuration_status == ConfigurationStatus::Ready
            && settings.generation == catalog.revision
            && let Err(error) = catalog.sample_counts(&mut client, domain, rows, &mut counts)
        {
            if client.snapshot()?.generation == catalog.revision {
                return Err(error);
            }
            counts.fill(None);
        }
        Ok((catalog, start, counts))
    }

    fn sample_counts(
        &self,
        client: &mut ManagementClient,
        domain: Option<&str>,
        rows: &[Entry],
        counts: &mut Counts,
    ) -> io::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let (revision, mut page) = client.firewall_counters(domain, Cursor::default())?;
        if revision != self.revision {
            return Ok(());
        }
        let mut offset = 0;
        for (entry, count) in rows.iter().zip(counts.iter_mut()) {
            // 旧双栈声明的计数是双栈合计，不能显示为任一地址族的计数。
            if rule_address(&entry.rule).is_none() {
                continue;
            }
            if entry.offset >= offset + page.rows.len() {
                offset = entry.offset;
                let cursor = Cursor {
                    revision: page.revision,
                    offset: u32::try_from(offset).map_err(io::Error::other)?,
                };
                let (revision, next) = client.firewall_counters(domain, cursor)?;
                if revision != self.revision {
                    counts.fill(None);
                    return Ok(());
                }
                page = next;
            }
            let Some(row) = page.rows.get(entry.offset - offset) else {
                continue;
            };
            if row.position as usize == entry.key.index + 1
                && row.chain.as_deref() == entry.key.chain.as_ref().map(ConfigurationName::as_str)
                && row.rule.disabled == entry.rule.disabled
            {
                *count = Some((row.bytes, row.packets));
            }
        }
        Ok(())
    }
}

fn read_configuration(
    client: &mut ManagementClient,
) -> io::Result<(Revision, RouterConfiguration)> {
    let (revision, bytes) = client.export()?;
    let configuration = RouterConfiguration::decode(&bytes).map_err(io::Error::other)?;
    Ok((revision, configuration))
}

fn missing() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "所选路由域、规则链或规则已不存在。",
    )
}

/// 基准代际确定链内身份。候选交给配置服务以同一代际提交。
pub(super) fn candidate(
    domain: Option<&str>,
    expected: Revision,
    change: Change,
) -> io::Result<Vec<u8>> {
    let (revision, mut configuration) = read_configuration(&mut ManagementClient::local())?;
    if revision != expected {
        return Err(io::Error::other("配置已变化，请从最新规则列表重新打开。"));
    }
    let firewall = if let Some(domain) = domain {
        &mut configuration
            .routing_domains
            .get_mut(domain)
            .ok_or_else(missing)?
            .firewall
    } else {
        &mut configuration.firewall
    };
    change.apply(firewall)?;
    configuration.encode().map_err(io::Error::other)
}

impl Change {
    fn apply(self, firewall: &mut Firewall) -> io::Result<()> {
        match self {
            Self::Add(rule) => firewall.prerouting.push(rule),
            Self::Put(key, rule) => key.rules(firewall)?[key.index] = rule,
            Self::Enable(key, enabled) => key.rules(firewall)?[key.index].disabled = !enabled,
            Self::Remove(key) => {
                key.rules(firewall)?.remove(key.index);
            }
        }
        Ok(())
    }
}

impl Key {
    fn rules<'a>(&self, firewall: &'a mut Firewall) -> io::Result<&'a mut Vec<PreroutingRule>> {
        let rules = if let Some(chain) = &self.chain {
            firewall.chains.get_mut(chain).ok_or_else(missing)?
        } else {
            &mut firewall.prerouting
        };
        if self.index >= rules.len() {
            return Err(missing());
        }
        Ok(rules)
    }
}
