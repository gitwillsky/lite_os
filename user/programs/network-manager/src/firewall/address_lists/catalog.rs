//! 地址列表是所选路由域防火墙配置的一部分，两个地址族共用同一份命名集合。

use crate::firewall::catalog::read_configuration;
use network_config::{
    ManagementClient, Revision,
    configuration::{ConfigurationName, Firewall, IpPrefix, connections::Family},
};
use std::{fmt, io};

#[derive(Clone, Eq, PartialEq)]
pub(super) struct Member {
    pub(super) name: ConfigurationName,
    pub(super) prefix: IpPrefix,
}

impl Member {
    pub(super) fn address(&self) -> impl fmt::Display + '_ {
        struct Address(IpPrefix);
        impl fmt::Display for Address {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let IpPrefix { network, length } = self.0;
                let host = if network.is_ipv4() { 32 } else { 128 };
                if length == host {
                    write!(f, "{network}")
                } else {
                    write!(f, "{network}/{length}")
                }
            }
        }
        Address(self.prefix)
    }
}

pub(super) struct Entry {
    pub(super) member: Member,
    pub(super) text: String,
}

pub(super) struct Catalog {
    pub(super) revision: Revision,
    pub(super) names: Vec<ConfigurationName>,
    pub(super) entries: Vec<Entry>,
}

impl Catalog {
    /// 代际未变化时，跳过完整配置读取与列表重建。
    ///
    /// 错误：管理服务无法读取配置，或所选路由域已删除。
    pub(super) fn load(
        domain: Option<&str>,
        family: Family,
        previous: Option<Revision>,
    ) -> io::Result<Option<Self>> {
        let mut client = ManagementClient::local();
        if previous == Some(client.snapshot()?.generation) {
            return Ok(None);
        }
        let (revision, config) = read_configuration(&mut client)?;
        let firewall = if let Some(domain) = domain {
            &config
                .routing_domains
                .get(domain)
                .ok_or_else(missing_domain)?
                .firewall
        } else {
            &config.firewall
        };
        let mut names = Vec::new();
        let mut entries = Vec::new();
        for (name, prefixes) in &firewall.address_sets {
            let mut matched: Vec<_> = prefixes
                .iter()
                .copied()
                .filter(|prefix| family.includes(prefix.network))
                .collect();
            if matched.is_empty() {
                continue;
            }
            matched.sort_unstable();
            matched.dedup();
            names.push(name.clone());
            for prefix in matched {
                let member = Member {
                    name: name.clone(),
                    prefix,
                };
                let text = format!("{}\t{}", name.as_str(), member.address());
                entries.push(Entry { member, text });
            }
        }
        Ok(Some(Self {
            revision,
            names,
            entries,
        }))
    }
}

/// 新建、移动或删除一个静态成员。相同名单和网段构成同一成员。
pub(super) enum Change {
    Put {
        original: Option<Member>,
        member: Member,
    },
    Remove(Member),
}

/// 从指定代际读取并修改所选域，返回交给配置事务的完整候选。
///
/// 错误：代际变化、域或条目消失、目标成员重复，或配置无法编码。
pub(super) fn candidate(
    domain: Option<&str>,
    expected: Revision,
    change: Change,
) -> io::Result<Vec<u8>> {
    let (revision, mut config) = read_configuration(&mut ManagementClient::local())?;
    if revision != expected {
        return Err(io::Error::other("配置已变化，请从最新地址列表重新打开。"));
    }
    let firewall = if let Some(domain) = domain {
        &mut config
            .routing_domains
            .get_mut(domain)
            .ok_or_else(missing_domain)?
            .firewall
    } else {
        &mut config.firewall
    };
    match change {
        Change::Put { original, member } => {
            if firewall
                .address_sets
                .get(&member.name)
                .is_some_and(|set| set.contains(&member.prefix))
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "该列表已有相同地址。",
                ));
            }
            // 先加入再移除，编辑同一列表最后一项时保留其他规则引用的集合。
            firewall
                .address_sets
                .entry(member.name)
                .or_default()
                .push(member.prefix);
            if let Some(original) = original {
                remove(firewall, &original)?;
            }
        }
        Change::Remove(member) => remove(firewall, &member)?,
    }
    config.encode().map_err(io::Error::other)
}

fn remove(firewall: &mut Firewall, member: &Member) -> io::Result<()> {
    let members = firewall
        .address_sets
        .get_mut(&member.name)
        .ok_or_else(missing_member)?;
    let previous = members.len();
    members.retain(|prefix| *prefix != member.prefix);
    if previous == members.len() {
        return Err(missing_member());
    }
    // 最后一个静态成员消失时，仍被引用的空集合继续匹配零个地址。
    if members.is_empty()
        && !firewall
            .rules()
            .any(|rule| rule.destination_set.as_ref() == Some(&member.name))
    {
        firewall.address_sets.remove(&member.name);
    }
    Ok(())
}

fn missing_domain() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "所选路由域已不存在。")
}

fn missing_member() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "所选地址已不存在。")
}

/// 列表使用规范网段表示地址，裸地址等价于主机长度。
pub(super) fn parse_address(text: &str) -> Option<IpPrefix> {
    let text = text.trim();
    if let Ok(network) = text.parse::<std::net::IpAddr>() {
        return Some(IpPrefix {
            network,
            length: if network.is_ipv4() { 32 } else { 128 },
        });
    }
    text.parse().ok()
}
