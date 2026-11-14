//! 合并各协议实际拥有的截止时间。只有收发积压清空后才扫描，不维持第二份计时器状态。

use super::{NetworkStack, device::ProtocolDevice, now};
use crate::namespace::{DomainScan, take_network_namespaces};
use native_runtime::native_abi::Status;
use ::tcp::time::Instant;

/// 协议推进线程下一次需要运行的绝对单调时间，单位纳秒。
/// 无已知计时器时返回无限期限。第二项标记未能检查的目录或域，调用者须重试。
/// 在没有目录或协议锁时向 `continue_scan` 交付当前最早期限。返回 false 时立即重进工作轮。
/// # Errors
/// 回调返回的线程等待错误。
pub(crate) fn network_poll_deadline(
    lane: usize,
    lanes: usize,
    mut continue_scan: impl FnMut(i64) -> Result<bool, Status>,
) -> Result<(i64, bool), Status> {
    let timestamp = now();
    let mut scan = DomainScan::new(lane, lanes);
    let mut next = native_runtime::native_abi::DEADLINE_INFINITE;
    let mut blocked = false;
    loop {
        let Some(batch) = take_network_namespaces(&mut scan) else {
            return Ok((next, true));
        };
        for namespace in batch.namespaces.into_iter().flatten() {
            let Some(stack) = namespace.protocol() else {
                continue;
            };
            let deadline = match stack.poll_deadline(timestamp) {
                Ok(Some(deadline)) => deadline.total_micros().saturating_mul(1000),
                Ok(None) => continue,
                Err(()) => {
                    blocked = true;
                    continue;
                }
            };
            next = next.min(deadline);
        }
        if !batch.backlog {
            return Ok((next, blocked));
        }
        if !continue_scan(next)? {
            return Ok((0, blocked));
        }
    }
}

impl NetworkStack {
    pub(super) fn next_deadline(&self, timestamp: Instant) -> Option<Instant> {
        let now_ms = timestamp.total_millis().max(0) as u64;
        let link_deadlines = self.interfaces.iter().flat_map(|interface| {
            let vlan = interface
                .device
                .vlan()
                .and_then(|link| link.next_deadline());
            let arp = (interface.index != 1
                && interface.link_active()
                && interface.state.address.is_some())
            .then(|| interface.arp.next_deadline())
            .flatten();
            let bridge_port = self.switching.contains(interface.index);
            let igmp = (!bridge_port)
                .then(|| interface.igmp.next_deadline())
                .flatten();
            let ipv6 = if !bridge_port && matches!(interface.device, ProtocolDevice::Ethernet(_)) {
                interface
                    .ipv6
                    .as_ref()
                    .and_then(|ipv6| ipv6.next_deadline(now_ms))
            } else {
                None
            };
            [arp, igmp, ipv6]
                .into_iter()
                .flatten()
                .filter_map(|ms| {
                    let micros = i64::try_from(ms.checked_mul(1000)?).ok()?;
                    Some(Instant::from_micros(micros))
                })
                .chain(vlan)
        });
        [
            self.reassembly.next_deadline(),
            self.switching.next_deadline(timestamp),
            self.identification.next_deadline(),
            self.route_replies.next_deadline(),
            self.udp_deadline(),
            self.forwarding.next_deadline(self.forwarding_enabled),
            self.tcp_deadline(timestamp),
            self.raw_deadline(),
        ]
        .into_iter()
        .flatten()
        .chain(link_deadlines)
        .min()
    }
}
