//! RFC 8106 通告事实及独立寿命。每条记录只属于接收接口。
use alloc::{string::String, vec::Vec};
use core::net::{Ipv6Addr, SocketAddrV6};
use network_protocol::SocketError;
use packet_wire::{dns_name::UncompressedNames, ndisc::options::OptionValue};
use router_protocol::dns::{LearnedDns, MAX_LEARNED, is_server};

const MAX_PER_LINK: usize = 8;

#[derive(Default)]
pub(super) struct Dns {
    servers: Vec<(Ipv6Addr, u64)>,
    domains: Vec<(String, u64)>,
}

impl Dns {
    pub(super) fn due(&self, now: u64) -> bool {
        self.servers.iter().any(|entry| entry.1 <= now)
            || self.domains.iter().any(|entry| entry.1 <= now)
    }

    pub(super) fn expire(&mut self, now: u64) -> bool {
        let before = self.servers.len() + self.domains.len();
        self.servers.retain(|entry| entry.1 > now);
        self.domains.retain(|entry| entry.1 > now);
        before != self.servers.len() + self.domains.len()
    }

    pub(super) fn receive(
        &mut self,
        option: OptionValue<'_>,
        now: u64,
        insertion: &mut (usize, usize),
    ) -> bool {
        let mut changed = false;
        match option {
            OptionValue::RecursiveDnsServers { lifetime, servers } => {
                if !servers.iter().copied().map(Ipv6Addr::from).all(is_server) {
                    return false;
                }
                let expires = super::discovery::deadline(now, lifetime);
                for address in servers.iter().copied().map(Ipv6Addr::from) {
                    changed |= update(
                        &mut self.servers,
                        &mut insertion.0,
                        expires,
                        lifetime == 0,
                        |old| *old == address,
                        || Some(address),
                    );
                }
            }
            OptionValue::DnsSearchList { lifetime, names } => {
                let expires = super::discovery::deadline(now, lifetime);
                changed = self.receive_names(names, expires, lifetime == 0, &mut insertion.1);
            }
            _ => {}
        }
        changed
    }

    fn receive_names(
        &mut self,
        names: UncompressedNames<'_>,
        expires: u64,
        withdraw: bool,
        at: &mut usize,
    ) -> bool {
        let mut changed = false;
        'names: for name in names {
            let mut text = [0; router_protocol::DNS_SUFFIX_MAX];
            let mut length = 0;
            // 线名称最长 255 字节，标签长度字节和根点省去两个字节。
            for label in name.labels() {
                if label.contains(&b'.') {
                    continue 'names;
                }
                if length != 0 {
                    text[length] = b'.';
                    length += 1;
                }
                text[length..length + label.len()].copy_from_slice(label);
                length += label.len();
            }
            let Ok(name) = router_protocol::dns_suffix(&text[..length]) else {
                continue;
            };
            if !name.is_empty() {
                changed |= update(
                    &mut self.domains,
                    at,
                    expires,
                    withdraw,
                    |old| old.eq_ignore_ascii_case(name),
                    || owned_name(name).ok(),
                );
            }
        }
        changed
    }

    /// 捕获当前生效的事实，链路本地地址带接收接口范围。
    pub(in crate::inet) fn capture(
        &self,
        index: u32,
        now: u64,
        output: &mut LearnedDns,
    ) -> Result<(), SocketError> {
        for &(address, expires) in &self.servers {
            let scope = if address.is_unicast_link_local() {
                index
            } else {
                0
            };
            let server = SocketAddrV6::new(address, 53, 0, scope);
            if expires > now
                && output.servers.len() < MAX_LEARNED
                && !output.servers.contains(&server)
            {
                output
                    .servers
                    .try_reserve(1)
                    .map_err(|_| SocketError::NoMemory)?;
                output.servers.push(server);
            }
        }
        for (name, expires) in &self.domains {
            if *expires > now
                && output.domains.len() < MAX_LEARNED
                && !output
                    .domains
                    .iter()
                    .any(|old| old.eq_ignore_ascii_case(name))
            {
                output
                    .domains
                    .try_reserve(1)
                    .map_err(|_| SocketError::NoMemory)?;
                output.domains.push(owned_name(name)?);
            }
        }
        Ok(())
    }
}

fn owned_name(name: &str) -> Result<String, SocketError> {
    let mut result = String::new();
    result
        .try_reserve_exact(name.len())
        .map_err(|_| SocketError::NoMemory)?;
    result.push_str(name);
    Ok(result)
}

fn update<T>(
    entries: &mut Vec<(T, u64)>,
    at: &mut usize,
    expires: u64,
    withdraw: bool,
    matches: impl Fn(&T) -> bool,
    value: impl FnOnce() -> Option<T>,
) -> bool {
    if let Some(index) = entries.iter().position(|entry| matches(&entry.0)) {
        if withdraw {
            entries.remove(index);
            *at -= usize::from(index < *at);
            return true;
        }
        entries[index].1 = expires;
        return false;
    }
    if withdraw {
        return false;
    }
    let victim = if entries.len() == MAX_PER_LINK {
        let (index, _) = entries
            .iter()
            .enumerate()
            .min_by_key(|(index, entry)| (entry.1, core::cmp::Reverse(*index)))
            .expect("full DNS list");
        if index < *at && entries[index].1 >= expires {
            return false;
        }
        Some(index)
    } else {
        None
    };
    let Some(value) = value() else { return false };
    if let Some(index) = victim {
        entries.remove(index);
        *at -= usize::from(index < *at);
    } else if entries.try_reserve(1).is_err() {
        return false;
    }
    entries.insert(*at, (value, expires));
    *at += 1;
    true
}
