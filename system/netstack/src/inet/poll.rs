use super::ingress::IngressRound;
use super::*;

/// 本机发送的包数上限和共享微秒截止。首个实际尝试保证推进，之后每八次采样一次时钟。
pub(super) fn spend_output_attempt(remaining: &mut usize, work_until: u64) -> bool {
    let spent = NETWORK_RX_BUDGET - *remaining;
    if *remaining == 0
        || spent % 8 == 1 && work_until != u64::MAX && get_time_us() >= work_until
    {
        return false;
    }
    *remaining -= 1;
    true
}
/// 一轮有界deferred推进；同域所有设备共享唯一传输端点/ports/cleanup。
pub(super) struct NetworkPoll {
    pub(super) backlog: bool,
    pub(super) packet_transmit_ready: [Option<u32>; 8],
    /// 每个实际设备本轮首个诊断；硬件错误不覆盖其他设备的edges/backlog，也不伪造socket错误。
    pub(super) device_failures: [Option<(u32, device_model::network::NetworkError)>; 8],
}
impl NetworkStack {
    /// 单一协议推进事务。协议时钟使用调用方传入的 `timestamp`。
    /// `work_until` 是微秒软截止，允许完成已领取的接收批次及本轮的发送和通知。
    pub(super) fn poll_until(
        &mut self,
        timestamp: Instant,
        work_until: u64,
        refresh_links: bool,
    ) -> NetworkPoll {
        self.snapshot_readiness();
        let link_changed = refresh_links && self.refresh_carriers();
        let mut backlog = self.switching.expire(timestamp, work_until);
        self.switching.tick_trees(&self.interfaces, timestamp);
        let mut link_changed = self.refresh_vlan_carriers() || link_changed;
        // 所有实际TCP slot先推进纯时间状态；无FIB/设备DOWN不阻止timeout或TIME_WAIT退休。
        self.maintain_tcp_clocks(timestamp);
        backlog |= self.retire_fragments(timestamp, work_until);
        backlog |= self.identification.retire(timestamp);
        backlog |= self.forwarding.retire_state(timestamp, work_until);
        let mut packet_transmit_ready = [None; 8];
        let mut ingress = IngressRound {
            receive: NETWORK_RX_BUDGET,
            // 本轮统一的转发 TX 预算；分片耗尽时即刻续调度，不能误用 ARP 的秒级重试延迟。
            transmit: NETWORK_RX_BUDGET,
            neighbor_progress: link_changed,
            addresses_changed: false,
        };
        let mut tree_tx = NETWORK_RX_BUDGET;
        let mut device_failures = [None; 8];
        let mut gateway_reachability_changed = false;
        let mut retry_attempts = 0;
        let interface_count = self.interfaces.len();
        let first = self.interface_cursor;
        let end = first.saturating_add(8).min(interface_count);
        let rotation = self.interface_rotation;
        // 先消费队列线程已发表的可写通知，再服务旧待办，避免新 RX 抢占发送预算。
        for cursor in first..end {
            let interface = &mut self.interfaces[(rotation + cursor) % interface_count];
            if interface.device.take_transmit_ready() {
                packet_transmit_ready[cursor - first] = Some(interface.index);
                ingress.neighbor_progress = true;
            }
            if !self.switching.contains(interface.index) {
                let (changed, pending) =
                    interface.poll_ipv6(timestamp, self.forwarding_enabled.ipv6);
                ingress.addresses_changed |= changed;
                ingress.neighbor_progress |= changed;
                backlog |= pending;
                backlog |= interface.poll_igmp(timestamp);
            }
        }
        self.switching
            .send_trees(&self.interfaces, timestamp, &mut tree_tx);
        backlog |= self
            .switching
            .send_gateways(&self.interfaces, timestamp, &mut ingress.transmit);
        if ingress.neighbor_progress {
            self.retry_output(timestamp);
            ingress.neighbor_progress = false;
        }
        if self.forwarding_enabled.ipv4 || self.forwarding_enabled.ipv6 {
            // 分片续发也只能用掉首批 RX 额度以外的部分；剩余额度在收包后仍可用于重试。
            let limit = ingress
                .transmit
                .saturating_sub(device_model::network::RECEIVE_BATCH_CAPACITY);
            let mut retry_budget = limit;
            self.forwarding.poll(
                &mut self.interfaces,
                (&self.routing, &self.ipv6_routing, &self.selection_rules),
                (&mut self.route_replies, (&self.tcp_ports, &self.udp_ports)),
                self.forwarding_enabled,
                timestamp,
                (&mut retry_budget, &mut retry_attempts, work_until),
            );
            ingress.transmit -= limit - retry_budget;
        }
        let mut receive_resume = None;
        for cursor in first..end {
            let position = (rotation + cursor) % interface_count;
            let interface = &mut self.interfaces[position];
            let loopback_writable_before = match &interface.device {
                device::ProtocolDevice::Loopback(device) => Some(device.can_transmit()),
                _ => None,
            };
            if !interface.state.up {
                continue;
            }
            let bridged = self.switching.contains(interface.index);
            let drained = if receive_resume.is_none() {
                loop {
                    let received = if bridged {
                        let (before, rest) = self.interfaces.split_at(position);
                        let (port, after) = rest.split_first().expect("bridge port exists");
                        self.switching.receive_batch(
                            port,
                            (before, after),
                            timestamp,
                            (&mut ingress.receive, &mut ingress.transmit),
                        )
                    } else {
                        self.receive_host_batch(position, timestamp, &mut ingress)
                    };
                    if !received {
                        break true;
                    }
                    if ingress.receive == 0 {
                        // 包数上限仍沿原窗口轮转，避免有余时钟的下一轮只访问尾部空设备。
                        receive_resume = Some(end);
                        break false;
                    }
                    if work_until != u64::MAX && get_time_us() >= work_until {
                        // 后续控制工作仍可推进，但下轮收包先服务当前接口之后的设备。
                        receive_resume = Some(cursor + 1);
                        break false;
                    }
                }
            } else {
                false
            };
            if bridged {
                continue;
            }
            if drained && let Some(ipv6) = &mut self.interfaces[position].ipv6 {
                ingress.addresses_changed |=
                    ipv6.confirm_dad(timestamp.total_millis().max(0) as u64);
            }
            let progress = self.interfaces[position].poll_ipv6_neighbors(
                &self.ipv6_routing,
                &self.routing,
                &self.gateway_probes,
                timestamp,
            );
            backlog |= progress.backlog;
            gateway_reachability_changed |= progress.reachability_changed;
            ingress.neighbor_progress |= progress.reachability_changed;
            ingress.neighbor_progress |=
                self.retire_failed_neighbor_datagrams(position, progress.failed, timestamp);
            if loopback_writable_before == Some(false)
                && let device::ProtocolDevice::Loopback(device) = &self.interfaces[position].device
                && device.can_transmit()
            {
                packet_transmit_ready[cursor - first] = Some(self.interfaces[position].index);
                ingress.neighbor_progress = true;
            }
        }
        let next = receive_resume.unwrap_or(end);
        self.interface_revisit |= receive_resume.is_some();
        if next == interface_count {
            backlog |= core::mem::take(&mut self.interface_revisit);
            self.interface_cursor = 0;
            self.interface_rotation = (rotation + 1) % interface_count;
        } else {
            backlog = true;
            self.interface_cursor = next;
        }
        // 一批 ARP 只扫描等待队列一次，避免每个包重复扫描有界待办。
        if ingress.neighbor_progress || ingress.addresses_changed {
            self.retry_output(timestamp);
        }
        self.switching
            .send_trees(&self.interfaces, timestamp, &mut tree_tx);
        let gateway_change = self.refresh_vlan_carriers();
        link_changed |= gateway_change;
        backlog |= gateway_change;
        if self.forwarding_enabled.ipv4 || self.forwarding_enabled.ipv6 {
            backlog |= self.forwarding.poll(
                &mut self.interfaces,
                (&self.routing, &self.ipv6_routing, &self.selection_rules),
                (&mut self.route_replies, (&self.tcp_ports, &self.udp_ports)),
                self.forwarding_enabled,
                timestamp,
                (&mut ingress.transmit, &mut retry_attempts, work_until),
            );
        }
        backlog |= self.poll_route_replies(timestamp, first, end, rotation);
        backlog |= self.poll_udp(timestamp, first, end, rotation, work_until);
        backlog |= self.poll_tcp(timestamp, first, end, rotation, work_until);
        backlog |= self.poll_raw(timestamp, first, end, rotation, work_until);
        // 首次 TX 在本轮登记解析，控制帧随后提交。各 transport 共用一次 ARP 时钟推进。
        let (arp_backlog, arp_changed) = self.poll_arp_batch(
            (first..end).map(|cursor| (rotation + cursor) % interface_count),
            timestamp,
        );
        backlog |= arp_backlog;
        backlog |= self
            .switching
            .send_gateways(&self.interfaces, timestamp, &mut ingress.transmit);
        gateway_reachability_changed |= arp_changed;
        for cursor in first..end {
            let interface = &self.interfaces[(rotation + cursor) % interface_count];
            if let Some(failure) = interface.device.take_error() {
                device_failures[cursor - first] = Some((interface.index, failure));
            }
        }
        // 队列故障可能先于独立链路事件被发现。只检查出错接口，资格变化才安排续轮。
        let mut failed_link_changed = false;
        for (index, _) in device_failures.iter().flatten() {
            if let Some(position) = NetworkInterface::position(&self.interfaces, *index) {
                failed_link_changed |= self.refresh_carrier_at(position);
            }
        }
        if failed_link_changed {
            self.retry_output(timestamp);
            link_changed = true;
            backlog = true;
        }
        self.switching
            .notify_gateway_transmit(&self.interfaces, &packet_transmit_ready);
        self.capture_readiness_transitions();
        if ingress.addresses_changed || link_changed {
            self.notify_interfaces();
        }
        if gateway_reachability_changed {
            self.notify_routing_inputs(network_protocol::NetworkInstaller::GATEWAYS_CHANGED);
        }
        NetworkPoll {
            backlog,
            packet_transmit_ready,
            device_failures,
        }
    }
}
