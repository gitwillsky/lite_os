//! 按实际本机接口与 Full 邻接生成本机 LSA。正文、序列和洪泛寿命都由数据库持有。

use super::{
    database::{Database, Domain, Identity},
    spf::Summary,
};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv6Addr};
use fallible_tree::FallibleMap;
use native_runtime::native_abi::{self, DEADLINE_INFINITE, Status};
use network_protocol::AddressSnapshot;
use packet_wire::ospf::{
    InterAreaPrefixLsa, IntraAreaPrefixLsa, Ipv6Prefix, LinkLsa, LsaKey, RouterLinkV2,
    RouterLinkV3, RouterLsaV2, RouterLsaV3, Version,
};

const IPV6_ROUTER_OPTIONS: u32 = 0x13;
// 分片的 Router-LSA 不超过最小 IPv6 MTU。64 条链路占 1024 字节，留出 IP/OSPF/LSA 报头空间。
const ROUTER_LINKS_PER_LSA: usize = 64;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) struct Attachment {
    pub(super) area: u32,
    pub(super) interface: u32,
    pub(super) address: IpAddr,
    pub(super) length: u8,
    pub(super) cost: u16,
    pub(super) passive: bool,
    pub(super) peer: Option<(u32, u32)>,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct Prefix {
    interface: u32,
    network: Ipv6Addr,
    length: u8,
}

pub(super) struct Origination {
    attachments: Vec<Attachment>,
    prefixes: Vec<Prefix>,
    scan: Vec<Prefix>,
    published: Vec<Identity>,
    next: Vec<Identity>,
    changes: Vec<Identity>,
    body: Vec<u8>,
    due: i64,
}

impl Default for Origination {
    fn default() -> Self {
        Self {
            attachments: Vec::new(),
            prefixes: Vec::new(),
            scan: Vec::new(),
            published: Vec::new(),
            next: Vec::new(),
            changes: Vec::new(),
            body: Vec::new(),
            due: DEADLINE_INFINITE,
        }
    }
}

impl Origination {
    pub(super) fn invalidate(&mut self, now: i64) {
        self.due = self.due.min(now);
    }

    pub(super) fn addresses(
        &mut self,
        addresses: &AddressSnapshot,
        eligible: impl Fn(u32) -> bool,
        now: i64,
    ) -> Result<(), Status> {
        self.scan.clear();
        for row in (0..addresses.len()).filter_map(|position| addresses.get(position)) {
            let IpAddr::V6(address) = row.address else {
                continue;
            };
            if !row.usable
                || !(row.on_link || row.prefix_length == 128)
                || !eligible(row.interface_index)
                || address.is_unicast_link_local()
                || address.is_unspecified()
                || address.is_loopback()
                || address.is_multicast()
                || address.to_ipv4_mapped().is_some()
            {
                continue;
            }
            let mask = u128::MAX
                .checked_shl(128 - u32::from(row.prefix_length))
                .unwrap_or(0);
            push(
                &mut self.scan,
                Prefix {
                    interface: row.interface_index,
                    network: Ipv6Addr::from(u128::from(address) & mask),
                    length: row.prefix_length,
                },
            )?;
        }
        self.scan.sort_unstable();
        self.scan.dedup();
        if self.scan != self.prefixes {
            core::mem::swap(&mut self.scan, &mut self.prefixes);
            self.invalidate(now);
        }
        Ok(())
    }

    pub(super) fn advance(
        &mut self,
        database: &mut Database,
        attachments: impl Iterator<Item = Attachment> + Clone,
        summaries: [&[Summary]; 2],
        border: [bool; 2],
        router: u32,
        now: i64,
    ) -> Result<(i64, &[Identity]), Status> {
        if !attachments.clone().eq(self.attachments.iter().copied()) {
            self.attachments.clear();
            self.attachments
                .try_reserve(attachments.clone().count())
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            self.attachments.extend(attachments);
            self.invalidate(now);
        }
        self.changes.clear();
        if now < self.due {
            return Ok((self.due, &self.changes));
        }

        self.next.clear();
        let mut area = Publication {
            database,
            router,
            now,
            body: &mut self.body,
            wanted: &mut self.next,
            changes: &mut self.changes,
            due: DEADLINE_INFINITE,
        };
        for links in self
            .attachments
            .chunk_by(|left, right| left.area == right.area)
        {
            area.ipv4(links, border[0])?;
            area.ipv6(links, &self.prefixes, border[1])?;
        }
        for (version, entries) in [Version::V2, Version::V3].into_iter().zip(summaries) {
            for target in entries.chunk_by(|left, right| left.area == right.area) {
                area.summaries(version, target)?;
            }
        }
        self.due = area.due;
        self.next.sort_unstable();
        for &key in &self.published {
            if self.next.binary_search(&key).is_err() && database.withdraw(key, now) {
                push(&mut self.changes, key)?;
            }
        }
        core::mem::swap(&mut self.published, &mut self.next);
        Ok((self.due, &self.changes))
    }
}

struct Publication<'a> {
    database: &'a mut Database,
    router: u32,
    now: i64,
    body: &'a mut Vec<u8>,
    wanted: &'a mut Vec<Identity>,
    changes: &'a mut Vec<Identity>,
    due: i64,
}

impl Publication<'_> {
    fn prepare(&mut self, length: usize) -> Result<&mut [u8], Status> {
        self.body
            .try_reserve(length.saturating_sub(self.body.len()))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        self.body.resize(length, 0);
        Ok(self.body)
    }

    fn publish(&mut self, domain: Domain, kind: u16, id: u32, size: usize) -> Result<(), Status> {
        let key = domain
            .identify(LsaKey {
                kind,
                link_state_id: id,
                advertising_router: self.router,
            })
            .expect("local LSA type and scope");
        let (changed, due) = self.database.originate(key, &self.body[..size], self.now)?;
        self.due = self.due.min(due);
        push(self.wanted, key)?;
        if changed {
            push(self.changes, key)?;
        }
        Ok(())
    }

    fn ipv4(&mut self, links: &[Attachment], border: bool) -> Result<(), Status> {
        let mut descriptions = Vec::new();
        for link in links {
            let IpAddr::V4(address) = link.address else {
                continue;
            };
            let address = u32::from(address);
            if let Some((peer, _)) = link.peer {
                push(
                    &mut descriptions,
                    RouterLinkV2 {
                        kind: 1,
                        link_id: peer,
                        link_data: address,
                        cost: link.cost,
                        tos_metrics: &[],
                    },
                )?;
            }
            let mask = u32::MAX
                .checked_shl(32 - u32::from(link.length))
                .unwrap_or(0);
            push(
                &mut descriptions,
                RouterLinkV2 {
                    kind: 3,
                    link_id: address & mask,
                    link_data: mask,
                    cost: link.cost,
                    tos_metrics: &[],
                },
            )?;
        }
        if descriptions.is_empty() {
            return Ok(());
        }
        descriptions.sort_unstable_by_key(|row| (row.kind, row.link_id, row.link_data, row.cost));
        descriptions.dedup_by_key(|row| (row.kind, row.link_id, row.link_data));
        let domain = Domain {
            version: Version::V2,
            area: links[0].area,
            interface: 0,
        };
        let size = RouterLsaV2::write(
            self.prepare(4 + descriptions.len() * 12)?,
            u8::from(border),
            &descriptions,
        )
        .map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        self.publish(domain, 1, self.router, size)
    }

    fn ipv6(
        &mut self,
        links: &[Attachment],
        prefixes: &[Prefix],
        border: bool,
    ) -> Result<(), Status> {
        let area = links[0].area;
        let mut descriptions = Vec::new();
        let mut connected = Vec::new();
        let mut local_prefixes = Vec::new();
        let mut active = false;
        for link in links {
            let IpAddr::V6(address) = link.address else {
                continue;
            };
            active = true;
            if let Some((router, interface)) = link.peer {
                push(
                    &mut descriptions,
                    RouterLinkV3 {
                        kind: 1,
                        cost: link.cost,
                        interface: link.interface,
                        neighbor_interface: interface,
                        neighbor_router: router,
                    },
                )?;
            }
            let domain = Domain {
                version: Version::V3,
                area,
                interface: link.interface,
            };
            let first = prefixes.partition_point(|row| row.interface < link.interface);
            connected.clear();
            local_prefixes.clear();
            for row in prefixes[first..]
                .iter()
                .take_while(|row| row.interface == link.interface)
            {
                let prefix = Ipv6Prefix {
                    network: row.network,
                    length: row.length,
                    options: 0,
                };
                push(&mut connected, (prefix, link.cost))?;
                if !link.passive {
                    push(&mut local_prefixes, prefix)?;
                }
            }
            if !link.passive {
                let size = LinkLsa::write(
                    self.prepare(24 + local_prefixes.len() * 20)?,
                    0,
                    IPV6_ROUTER_OPTIONS,
                    address,
                    &local_prefixes,
                )
                .map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
                self.publish(domain, 8, link.interface, size)?;
            }
            if !connected.is_empty() {
                let reference = LsaKey {
                    kind: 0x2001,
                    link_state_id: 0,
                    advertising_router: self.router,
                };
                let size = IntraAreaPrefixLsa::write(
                    self.prepare(12 + connected.len() * 20)?,
                    reference,
                    &connected,
                )
                .map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
                self.publish(domain, 0x2009, link.interface, size)?;
            }
        }
        if !active {
            return Ok(());
        }
        descriptions.sort_unstable_by_key(|row| row.interface);
        let domain = Domain {
            version: Version::V3,
            area,
            interface: 0,
        };
        if descriptions.is_empty() {
            let size =
                RouterLsaV3::write(self.prepare(4)?, u8::from(border), IPV6_ROUTER_OPTIONS, &[])
                    .map_err(|_| native_abi::ERR_BAD_STATE)?;
            return self.publish(domain, 0x2001, 0, size);
        }
        for (id, rows) in descriptions.chunks(ROUTER_LINKS_PER_LSA).enumerate() {
            let size = RouterLsaV3::write(
                self.prepare(4 + rows.len() * 16)?,
                u8::from(border),
                IPV6_ROUTER_OPTIONS,
                rows,
            )
            .map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
            self.publish(domain, 0x2001, id as u32, size)?;
        }
        Ok(())
    }

    fn summaries(&mut self, version: Version, entries: &[Summary]) -> Result<(), Status> {
        let domain = Domain {
            version,
            area: entries[0].area,
            interface: 0,
        };
        let mut used = FallibleMap::default();
        let mut v4 = Vec::new();
        let entries = if version == Version::V2 {
            v4.try_reserve_exact(entries.len())
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            v4.extend_from_slice(entries);
            // 更具体的前缀可选 LS ID 更少，先为它们保留合法地址。
            v4.sort_unstable_by_key(|entry| {
                (
                    core::cmp::Reverse(entry.prefix.length),
                    entry.prefix.network,
                )
            });
            &v4
        } else {
            entries
        };
        for entry in entries {
            let (kind, id) = match entry.prefix.network {
                IpAddr::V4(network) => {
                    let first = u32::from(network);
                    let last = first
                        | u32::MAX
                            .checked_shr(u32::from(entry.prefix.length))
                            .unwrap_or(0);
                    let id = if !used.contains_key(&first) {
                        Some(first)
                    } else if !used.contains_key(&last) {
                        Some(last)
                    } else {
                        (first..last).find(|id| !used.contains_key(id))
                    };
                    // 若全区间均被更具体前缀占用，已有通告覆盖该网段。
                    let Some(id) = id else { continue };
                    (3, id)
                }
                IpAddr::V6(network) => {
                    // 依前缀计算 FNV-1a 初值。常规更新保留 LS ID，碰撞时用区域内的下一个可用值。
                    let mut id = network
                        .octets()
                        .into_iter()
                        .chain([entry.prefix.length])
                        .fold(0x811c9dc5u32, |hash, byte| {
                            (hash ^ u32::from(byte)).wrapping_mul(0x01000193)
                        });
                    while used.contains_key(&id) {
                        id = id.wrapping_add(1);
                    }
                    (0x2003, id)
                }
            };
            used.try_insert(id, ())
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            let lsa = InterAreaPrefixLsa {
                network: entry.prefix.network,
                length: entry.prefix.length,
                metric: entry.metric,
            };
            let size = lsa
                .write(self.prepare(24)?)
                .map_err(|_| native_abi::ERR_BAD_STATE)?;
            self.publish(domain, kind, id, size)?;
        }
        Ok(())
    }
}

fn push<T>(items: &mut Vec<T>, value: T) -> Result<(), Status> {
    items
        .try_reserve(1)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    items.push(value);
    Ok(())
}
