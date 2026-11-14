//! 在当前 NET guard 内读取一页 FIB 路径，完整编码放在 guard 外。

use super::NetworkInterface;
use crate::{inet::stack, namespace::Namespace};
use alloc::vec::Vec;
use core::net::IpAddr;
use network_protocol::{
    SocketError,
    routes::{NextHopMember, RouteAddress, RouteAttributes},
};
use router_protocol::{
    route_view::{Cursor, LIMIT, NextHop, Page, Position, Row},
    routing::Family,
    tracking::IpPrefix,
};

/// 捕获单族的有界生效路径页，不执行选路、地址解析或设备探测。
/// # Errors
/// 域、路径位置、存储或编码失败。
pub(crate) fn route_page(scope: &Namespace, cursor: Cursor) -> Result<Vec<u8>, SocketError> {
    let mut rows = Vec::new();
    rows.try_reserve_exact(LIMIT)
        .map_err(|_| SocketError::NoMemory)?;
    let mut capture = Capture {
        cursor,
        page: Page { rows, next: None },
    };
    {
        let network = stack(scope)?.lock()?;
        match cursor.family {
            Family::Ipv4 => network
                .routing
                .fib
                .capture_page(&mut capture, &network.interfaces)?,
            Family::Ipv6 => network
                .ipv6_routing
                .capture_page(&mut capture, &network.interfaces)?,
        }
    }
    capture
        .page
        .encode(cursor)
        .map_err(|_| SocketError::Invalid)
}

/// 同一有界读取的累加器；不缓存完整路由、下一跳字典或接口。
pub(super) struct Capture {
    cursor: Cursor,
    page: Page,
}

impl Capture {
    pub(super) fn start(&self) -> usize {
        self.cursor.start.route as usize
    }

    pub(super) fn route<A: RouteAddress + Into<IpAddr>>(
        &mut self,
        route: usize,
        attributes: RouteAttributes<A>,
        members: &[NextHopMember<A>],
        interfaces: &[NetworkInterface],
    ) -> Result<bool, SocketError> {
        let route = u32::try_from(route).map_err(|_| SocketError::NoSpace)?;
        let start = if route == self.cursor.start.route {
            self.cursor.start.member as usize
        } else {
            0
        };
        let row = Row {
            position: Position { route, member: 0 },
            prefix: IpPrefix {
                network: attributes.destination.into(),
                length: attributes.prefix_length,
            },
            table: attributes.table,
            metric: attributes.metric,
            scope: attributes.scope,
            protocol: attributes.protocol,
            kind: attributes.kind,
            preferred_source: attributes.preferred_source.map(Into::into),
            hop: None,
        };
        if members.is_empty() {
            return Ok(start == 0 && self.push(row));
        }
        let mut position = start;
        while let Some(hop) = members.get(position) {
            let index =
                match interfaces.binary_search_by_key(&hop.interface_index, |entry| entry.index) {
                    Ok(index) => index,
                    Err(next) => {
                        // 共享 IPv6 组可保留已拔出的成员。两侧按接口号有序，跳过缺席出口段。
                        let Some(interface) = interfaces.get(next) else {
                            break;
                        };
                        position += members[position..]
                            .partition_point(|member| member.interface_index < interface.index);
                        continue;
                    }
                };
            let member = u32::try_from(position).map_err(|_| SocketError::NoSpace)?;
            if self.push(Row {
                position: Position { route, member },
                hop: Some(NextHop {
                    interface: hop.interface_index,
                    name: interfaces[index].name,
                    gateway: hop.gateway.map(Into::into),
                    weight: hop.weight,
                    on_link: hop.on_link,
                }),
                ..row
            }) {
                return Ok(true);
            }
            position += 1;
        }
        Ok(false)
    }

    fn push(&mut self, row: Row) -> bool {
        if self.page.rows.len() == LIMIT {
            self.page.next = Some(row.position);
            return true;
        }
        self.page.rows.push(row);
        false
    }
}
