use smoltcp::iface::PollIngressSingleResult;

use super::*;

/// 一轮有界 deferred network poll 的调度结果。
pub(super) struct NetworkPoll {
    /// RX 或 completion backlog 仍需重新投递。
    pub(super) backlog: bool,
    /// TX capacity 从零变为可用，需要唤醒 packet sender。
    pub(super) transmit_became_available: bool,
}

impl NetworkStack {
    /// 有界推进 completion、ingress、协议维护与 egress。
    ///
    /// 任一 adapter failure 先锁存到 Ethernet owner，再返回 typed error。
    pub(super) fn poll(&mut self) -> Result<NetworkPoll, crate::drivers::network::NetworkError> {
        self.snapshot_readiness();
        let completion = match self.device.poll_completions(NETWORK_TX_COMPLETION_BUDGET) {
            Ok(completion) => completion,
            Err(error) => {
                self.capture_readiness_transitions();
                return Err(error);
            }
        };
        let timestamp = now();
        // 1. 定时维护只执行一次，确保单轮协议推进的固定成本。
        self.interface.poll_maintenance(timestamp);
        // 2. ingress 逐帧推进并受 budget 限制，禁止网络洪泛独占当前 CPU。
        let mut rx_budget_exhausted = true;
        for _ in 0..NETWORK_RX_BUDGET {
            if self
                .interface
                .poll_ingress_single(timestamp, &mut self.device, &mut self.sockets)
                == PollIngressSingleResult::None
            {
                rx_budget_exhausted = false;
                break;
            }
        }
        tcp::maintain(self);
        // 3. egress API 自身保证有界；在 ingress 后推进一次即可发送 ARP/UDP 响应。
        self.interface
            .poll_egress(timestamp, &mut self.device, &mut self.sockets);
        tcp::reap_orphans(self);
        if let Some(error) = self.device.pending_error() {
            self.capture_readiness_transitions();
            return Err(error);
        }
        if let Err(error) = self.device.finish_receive_batch() {
            self.capture_readiness_transitions();
            return Err(error);
        }
        self.capture_readiness_transitions();
        Ok(NetworkPoll {
            backlog: rx_budget_exhausted || completion.backlog,
            transmit_became_available: completion.transmit_became_available,
        })
    }
}
