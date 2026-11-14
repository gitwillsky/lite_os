//! 设备 RX 的固定批借用；协议状态仍按帧推进，未消费前缀/后缀在错误或退出时恰好归还一次。
use super::{
    EthernetDevice, EthernetReceive, EthernetReservation, EthernetRxToken, EthernetTxToken,
    NetworkError, NetworkReceiveFrame, ProtocolRxToken, ProtocolTxToken, VlanLink,
    loopback::LoopbackDevice,
};
use device_model::network::RECEIVE_BATCH_CAPACITY;
use packet_wire::batch::IngressIp;

pub(in crate::inet) enum ProtocolReceiveBatch<'a, 'b> {
    Ethernet(EthernetReceiveBatch<'a, 'b>),
    Vlan {
        device: &'a EthernetDevice,
        link: &'a VlanLink,
        remaining: usize,
    },
    Loopback {
        device: &'a LoopbackDevice,
        remaining: usize,
    },
}
impl<'a> Iterator for ProtocolReceiveBatch<'a, '_> {
    type Item = (ProtocolRxToken<'a>, ProtocolTxToken<'a>);
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Ethernet(batch) => batch
                .next()
                .map(|(rx, tx)| (ProtocolRxToken::Ethernet(rx), ProtocolTxToken::Ethernet(tx))),
            Self::Vlan {
                device,
                link,
                remaining,
            } => {
                if *remaining == 0 {
                    return None;
                }
                *remaining -= 1;
                device
                    .receive_vlan(link)
                    .map(|(rx, tx)| (ProtocolRxToken::Ethernet(rx), ProtocolTxToken::Ethernet(tx)))
            }
            Self::Loopback { device, remaining } => {
                if *remaining == 0 {
                    return None;
                }
                *remaining -= 1;
                device
                    .receive_one()
                    .map(|(rx, tx)| (ProtocolRxToken::Loopback(rx), ProtocolTxToken::Loopback(tx)))
            }
        }
    }
}

pub(in crate::inet) struct EthernetReceiveBatch<'a, 'b> {
    device: &'a EthernetDevice,
    // OWNER: 本批唯一持有的 CPU RX token；不复制帧、不分配堆、不增加逐帧 Arc 引用。
    frames: &'b mut [Option<NetworkReceiveFrame<'a>>],
    next: usize,
    count: usize,
    // 只有主机入口需要配对回包窗口。二层端口不预约入口 TX。
    host_ingress: bool,
}
impl EthernetDevice {
    pub(super) fn receive_vlan<'a>(
        &'a self,
        link: &'a VlanLink,
    ) -> Option<(EthernetRxToken<'a>, EthernetTxToken<'a>)> {
        let queue = link.receive()?;
        let ip = IngressIp::from_ethernet(queue.front()?);
        Some((
            EthernetRxToken {
                packets: &self.packets,
                interface_index: self.interface_index,
                mac: self.mac_address(),
                frame: EthernetReceive::Vlan { queue, ip },
                received_errors: &self.received_errors,
                pending_error: &self.pending_error,
            },
            self.transmit_token(Some(EthernetReservation::Reply(self, 0))),
        ))
    }

    pub(super) fn receive_batch<'a, 'b>(
        &'a self,
        frames: &'b mut [Option<NetworkReceiveFrame<'a>>],
        host_ingress: bool,
    ) -> Option<EthernetReceiveBatch<'a, 'b>> {
        let limit = frames.len().min(RECEIVE_BATCH_CAPACITY);
        let nic = self.nic()?;
        if limit == 0 {
            return None;
        }
        let slots = &mut frames[..limit];
        let queues = nic.queue_count();
        // 一个批次只借一个真实队列。设备错误或坏前缀不能留下已领取的票据。
        let error = match nic.receive_batch(self.receive_queue.get(), slots) {
            Ok(Some((queue, count)))
                if queue < queues
                    && (1..=limit).contains(&count)
                    && slots[..count].iter().all(Option::is_some)
                    && slots[count..].iter().all(Option::is_none) =>
            {
                self.receive_queue.set((queue + 1) % queues);
                return Some(EthernetReceiveBatch {
                    device: self,
                    frames,
                    next: 0,
                    count,
                    host_ingress,
                });
            }
            Ok(None) if slots.iter().all(Option::is_none) => return None,
            Ok(_) => NetworkError::Device,
            Err(error) => error,
        };
        self.record_receive_error(error);
        self.discard_received(slots);
        None
    }

    fn discard_received(&self, frames: &mut [Option<NetworkReceiveFrame<'_>>]) {
        for frame in frames.iter_mut().filter_map(Option::take) {
            if let Err(error) = frame.release() {
                self.record_receive_error(error);
            }
            self.received_dropped
                .set(self.received_dropped.get().saturating_add(1));
        }
    }
}
impl<'a> Iterator for EthernetReceiveBatch<'a, '_> {
    type Item = (EthernetRxToken<'a>, EthernetTxToken<'a>);
    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.count {
            return None;
        }
        let frame = self.frames[self.next]
            .take()
            .expect("validated batch prefix");
        self.next += 1;
        let device = self.device;
        let reply = self
            .host_ingress
            .then(|| EthernetReservation::Reply(device, frame.queue()));
        Some((
            EthernetRxToken {
                packets: &device.packets,
                interface_index: device.interface_index,
                mac: device.mac_address(),
                frame: EthernetReceive::Physical(frame),
                received_errors: &device.received_errors,
                pending_error: &device.pending_error,
            },
            device.transmit_token(reply),
        ))
    }
}
impl Drop for EthernetReceiveBatch<'_, '_> {
    fn drop(&mut self) {
        // 部分失败或提前停止不能把未消费槽留成 Borrowed；每个 token 仍只归还自己的 ticket。
        self.device.discard_received(self.frames);
    }
}
