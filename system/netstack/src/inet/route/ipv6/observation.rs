//! IPv6 路由观察使用活动 FIB 与接口上的通告状态，分页和整表快照沿相同投影。

use super::*;
use crate::inet::{ipv6::LearnedRoute, route::observation::Capture};
use network_protocol::routes::{NextHopConfiguration, NextHopMember};

fn learned_routes(
    interfaces: &[NetworkInterface],
) -> impl Iterator<Item = (u32, LearnedRoute)> + Clone {
    let now = crate::inet::now().total_millis().max(0) as u64;
    interfaces.iter().flat_map(move |interface| {
        let usable = Ipv6InterfaceFacts::capture(interface).usable();
        interface.ipv6.iter().flat_map(move |owner| {
            owner
                .learned_routes(now)
                .filter(move |route| route.protocol != 16 || usable)
                .map(move |route| (interface.index, route))
        })
    })
}

fn learned_attributes(route: LearnedRoute) -> RouteAttributes<Ipv6Addr> {
    RouteAttributes {
        destination: route.network,
        prefix_length: route.length,
        table: if route.local { TABLE_LOCAL } else { TABLE_MAIN },
        metric: route.metric,
        scope: if route.local {
            254
        } else if route.gateway.is_none() {
            253
        } else {
            0
        },
        protocol: route.protocol,
        kind: if route.local {
            RouteKind::Local
        } else {
            RouteKind::Unicast
        },
        preferred_source: None,
        metrics: RouteMetrics::default(),
    }
}
impl Ipv6Routes {
    pub(in crate::inet::route) fn capture_page(
        &self,
        capture: &mut Capture,
        interfaces: &[NetworkInterface],
    ) -> Result<(), SocketError> {
        let start = capture.start();
        for (position, entry) in self.entries.iter().enumerate().skip(start) {
            let single;
            let members = match entry.path {
                Path::Local(index) | Path::Direct(index) => {
                    single = NextHopMember {
                        interface_index: index,
                        gateway: None,
                        weight: 1,
                        on_link: false,
                    };
                    core::slice::from_ref(&single)
                }
                Path::Managed(id) => self
                    .groups
                    .as_ref()
                    .expect("installed paths")
                    .groups
                    .members(id),
                Path::Reject => &[],
            };
            if capture.route(position, entry.route, members, interfaces)? {
                return Ok(());
            }
        }
        // 通告和委派条目属于实际接口的寿命状态，在静态 FIB 之后按同一既有顺序观察。
        for (offset, (interface, route)) in learned_routes(interfaces)
            .enumerate()
            .skip(start.saturating_sub(self.entries.len()))
        {
            let member = NextHopMember {
                interface_index: interface,
                gateway: route.gateway,
                weight: 1,
                on_link: false,
            };
            if capture.route(
                self.entries.len() + offset,
                learned_attributes(route),
                &[member],
                interfaces,
            )? {
                break;
            }
        }
        Ok(())
    }

    /// 捕获当前根及完整路径组；热拔出后只投影仍存在的成员，不复制逐前缀路径或暴露槽身份。
    pub(in crate::inet) fn snapshot_bytes(
        &self,
        interfaces: &[NetworkInterface],
    ) -> Result<Vec<u8>, SocketError> {
        let mut automatic = Vec::new();
        automatic
            .try_reserve_exact(interfaces.len())
            .map_err(|_| SocketError::NoMemory)?;
        automatic.resize(interfaces.len(), u32::MAX);
        let mut managed = Vec::new();
        managed
            .try_reserve_exact(
                self.groups
                    .as_ref()
                    .map_or(0, |paths| paths.groups.slot_count()),
            )
            .map_err(|_| SocketError::NoMemory)?;
        managed.resize(
            self.groups
                .as_ref()
                .map_or(0, |paths| paths.groups.slot_count()),
            u32::MAX,
        );
        let position = |index| {
            interfaces
                .binary_search_by_key(&index, |interface| interface.index)
                .expect("live IPv6 route interface")
        };
        for entry in &self.entries {
            match entry.path {
                Path::Local(index) | Path::Direct(index) => automatic[position(index)] = 0,
                Path::Managed(id) => managed[id.get() as usize - 1] = 0,
                Path::Reject => {}
            }
        }
        let present = |index| {
            interfaces
                .binary_search_by_key(&index, |interface| interface.index)
                .is_ok()
        };
        let automatic_count = automatic.iter().filter(|index| **index == 0).count();
        let member_count = self
            .groups
            .iter()
            .flat_map(|paths| paths.groups.active_groups())
            .filter(|(id, _)| managed[id.get() as usize - 1] == 0)
            .try_fold(automatic_count, |count, (_, members)| {
                count
                    .checked_add(
                        members
                            .iter()
                            .filter(|member| present(member.interface_index))
                            .count(),
                    )
                    .ok_or(SocketError::NoSpace)
            })?;
        // 临时连续成员只按路径组捕获；不可变字典仍可含已退出成员，不能把它们写进新快照。
        let mut members = Vec::new();
        members
            .try_reserve_exact(member_count)
            .map_err(|_| SocketError::NoMemory)?;
        let mut ranges = Vec::new();
        ranges
            .try_reserve_exact(
                automatic_count
                    .checked_add(managed.iter().filter(|index| **index == 0).count())
                    .ok_or(SocketError::NoSpace)?,
            )
            .map_err(|_| SocketError::NoMemory)?;
        for (ordinal, index) in automatic
            .iter_mut()
            .enumerate()
            .filter(|(_, index)| **index == 0)
        {
            *index = u32::try_from(ranges.len()).map_err(|_| SocketError::NoSpace)?;
            let start = members.len();
            members.push(NextHopMember {
                interface_index: interfaces[ordinal].index,
                gateway: None,
                on_link: false,
                weight: 1,
            });
            ranges.push(start..members.len());
        }
        for (id, paths) in self
            .groups
            .iter()
            .flat_map(|paths| paths.groups.active_groups())
        {
            let index = &mut managed[id.get() as usize - 1];
            if *index != 0 {
                continue;
            }
            *index = u32::try_from(ranges.len()).map_err(|_| SocketError::NoSpace)?;
            let start = members.len();
            members.extend(
                paths
                    .iter()
                    .filter(|member| present(member.interface_index))
                    .copied(),
            );
            assert!(
                members.len() > start,
                "live IPv6 route has no remaining path"
            );
            ranges.push(start..members.len());
        }
        // 通告候选归接口的寿命状态，快照在同一 guard 中捕获其当前路径。
        let learned = learned_routes(interfaces);
        let count = learned.clone().count();
        u32::try_from(ranges.len().saturating_add(count)).map_err(|_| SocketError::NoSpace)?;
        let first_learned_group = ranges.len();
        members
            .try_reserve_exact(count)
            .map_err(|_| SocketError::NoMemory)?;
        ranges
            .try_reserve_exact(count)
            .map_err(|_| SocketError::NoMemory)?;
        for (interface_index, route) in learned.clone() {
            let start = members.len();
            members.push(NextHopMember {
                interface_index,
                gateway: route.gateway,
                on_link: false,
                weight: 1,
            });
            ranges.push(start..members.len());
        }
        let learned_rows = learned.enumerate().map(|(offset, (_, route))| {
            (
                learned_attributes(route),
                Some((first_learned_group + offset) as u32),
                false,
            )
        });
        network_protocol::RoutingSnapshot::encode(
            interfaces
                .iter()
                .map(|interface| (interface.index, interface.name)),
            ranges.iter().enumerate().map(|(index, range)| {
                NextHopConfiguration::new(index as u64 + 1, &members[range.clone()])
            }),
            self.entries
                .iter()
                .map(|entry| {
                    let group = match entry.path {
                        Path::Local(index) | Path::Direct(index) => {
                            Some(automatic[position(index)])
                        }
                        Path::Managed(id) => Some(managed[id.get() as usize - 1]),
                        Path::Reject => None,
                    };
                    (entry.route, group, false)
                })
                .chain(learned_rows),
        )
        .map_err(|error| {
            if error == native_runtime::native_abi::ERR_NO_MEMORY {
                SocketError::NoMemory
            } else {
                SocketError::Invalid
            }
        })
    }
}
