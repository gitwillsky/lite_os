//! 设备 RX 的固定批借用；协议状态仍按帧推进，未消费前缀/后缀在错误或退出时恰好归还一次。
use super::ingress::IngressFacts;
use super::{
    EthernetDevice, EthernetRxToken, EthernetTxToken, NetworkError, NetworkReceiveFrame,
    ProtocolRxToken, ProtocolTxToken, device_error, loopback::LoopbackDevice,
};
use device_model::network::RECEIVE_BATCH_CAPACITY;
use packet_wire::{
    PacketError,
    batch::{FrameError, parse_ethernet},
};

pub(in crate::inet) enum ProtocolReceiveBatch<'a, 'b> {
    Ethernet(EthernetReceiveBatch<'a, 'b>),
    Loopback {
        device: &'a LoopbackDevice,
        remaining: usize,
    },
}
impl<'a> Iterator for ProtocolReceiveBatch<'a, '_> {
    type Item = Result<(ProtocolRxToken<'a>, ProtocolTxToken<'a>), NetworkError>;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Ethernet(batch) => batch.next().map(|result| {
                result
                    .map(|(rx, tx)| (ProtocolRxToken::Ethernet(rx), ProtocolTxToken::Ethernet(tx)))
            }),
            Self::Loopback { device, remaining } => {
                if *remaining == 0 {
                    return None;
                }
                *remaining -= 1;
                device.receive_one().map(|(rx, tx)| {
                    Ok((ProtocolRxToken::Loopback(rx), ProtocolTxToken::Loopback(tx)))
                })
            }
        }
    }
}

pub(in crate::inet) struct EthernetReceiveBatch<'a, 'b> {
    device: &'a EthernetDevice,
    queue: u16,
    // OWNER: 本批唯一持有的 CPU RX token；不复制帧、不分配堆、不增加逐帧 Arc 引用。
    frames: &'b mut [Option<NetworkReceiveFrame<'a>>],
    // OWNER: 仅在本批存续的格式事实，来自仍被 CPU 独占的帧；不跨批缓存或持有额外帧引用。
    facts: &'b [Option<IngressFacts>],
    next: usize,
    count: usize,
}
impl EthernetDevice {
    pub(super) fn receive_batch<'a, 'b>(
        &'a self,
        frames: &'b mut [Option<NetworkReceiveFrame<'a>>],
        facts: &'b mut [Option<IngressFacts>],
    ) -> Option<EthernetReceiveBatch<'a, 'b>> {
        let limit = frames.len().min(RECEIVE_BATCH_CAPACITY);
        if limit == 0 {
            return None;
        }
        assert!(
            facts.len() >= limit,
            "batch header storage must cover the receive window"
        );
        for _ in 0..self.device.queue_count() {
            let queue = self.receive_queue.get();
            self.receive_queue
                .set((queue + 1) % self.device.queue_count());
            let count = match self.device.receive_batch(queue, &mut frames[..limit]) {
                Ok(count) => count,
                Err(error) => {
                    self.record_receive_error(error);
                    self.discard_received(frames);
                    return None;
                }
            };
            if count > limit
                || frames[..count].iter().any(Option::is_none)
                || frames[count..].iter().any(Option::is_some)
            {
                self.record_receive_error(NetworkError::Device);
                self.discard_received(frames);
                return None;
            }
            if count != 0 {
                {
                    let mut parsed =
                        [Err(FrameError::Packet(PacketError::Truncated)); RECEIVE_BATCH_CAPACITY];
                    parse_ethernet(
                        frames[..count]
                            .iter()
                            .map(|frame| &**frame.as_ref().expect("validated batch prefix")),
                        &mut parsed[..count],
                    )
                    .expect("header output uses the same batch length");
                    for (fact, parsed) in facts.iter_mut().zip(parsed[..count].iter()) {
                        *fact = parsed.ok().and_then(IngressFacts::from_frame);
                    }
                }
                return Some(EthernetReceiveBatch {
                    device: self,
                    queue,
                    frames,
                    facts,
                    next: 0,
                    count,
                });
            }
        }
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
    type Item = Result<(EthernetRxToken<'a>, EthernetTxToken<'a>), NetworkError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.count {
            return None;
        }
        let frame = self.frames[self.next]
            .take()
            .expect("validated batch prefix");
        let facts = self.facts[self.next];
        self.next += 1;
        let device = self.device;
        let reservation = match device_error::classify_optional(
            device.device.reserve_transmit(self.queue),
            |error| *error == NetworkError::WouldBlock,
        ) {
            Ok(reservation) => reservation,
            Err(error) => {
                device.record_transmit_error(error);
                // 出错后不继续消费该批；已领取帧与剩余帧的归还都发生在设备短锁之外。
                self.next = self.count;
                if let Err(error) = frame.release() {
                    device.record_receive_error(error);
                }
                device
                    .received_dropped
                    .set(device.received_dropped.get().saturating_add(1));
                return Some(Err(error));
            }
        };
        Some(Ok((
            EthernetRxToken {
                net_id: device.net_id,
                interface_index: device.interface_index,
                mac: device.device.mac_address(),
                frame,
                facts,
                received_errors: &device.received_errors,
                pending_error: &device.pending_error,
            },
            EthernetTxToken {
                net_id: device.net_id,
                interface_index: device.interface_index,
                reservation,
                checksum: false,
                maximum_frame_length: device.mtu + 14,
                dropped_transmits: &device.dropped_transmits,
                transmitted_errors: &device.transmitted_errors,
                pending_error: &device.pending_error,
            },
        )))
    }
}
impl Drop for EthernetReceiveBatch<'_, '_> {
    fn drop(&mut self) {
        // 部分失败或提前停止不能把未消费槽留成 Borrowed；每个 token 仍只归还自己的 ticket。
        self.device.discard_received(self.frames);
    }
}
