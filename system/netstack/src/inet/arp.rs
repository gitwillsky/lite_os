//! Ethernet/IPv4 ARP 准入与报文。邻接状态和探测预算复用同一原生邻居表。
use super::{
    InterfaceState,
    device::{ProtocolDevice, ProtocolTxToken},
    neighbor::NeighborCache,
};
use core::net::Ipv4Addr;
use device_model::network::NetworkError;
use packet_wire::{
    arp::{ArpHeader, ArpPacket},
    ethernet::{EthernetFrame, EthernetHeader},
};
use ::tcp::time::Instant;

const ARP: u16 = 0x0806;
const REQUEST: u16 = 1;
const REPLY: u16 = 2;
const BROADCAST: [u8; 6] = [255; 6];

/// 解析实际下一跳。广播和组播使用标准映射，不占用邻居表。
pub(super) fn resolve(
    cache: &mut NeighborCache<Ipv4Addr>,
    state: InterfaceState,
    destination: Ipv4Addr,
    next_hop: Ipv4Addr,
    now: Instant,
) -> Result<[u8; 6], NetworkError> {
    if destination.is_broadcast()
        || !state.broadcast.is_unspecified() && destination == state.broadcast
    {
        return Ok(BROADCAST);
    }
    if destination.is_multicast() {
        let address = destination.octets();
        return Ok([1, 0, 0x5e, address[1] & 0x7f, address[2], address[3]]);
    }
    if state.address.is_none() || !unicast(next_hop) {
        return Err(NetworkError::WouldBlock);
    }
    cache
        .resolve(next_hop, now.total_millis().max(0) as u64, |_| false)
        .map_err(|_| NetworkError::NoMemory)?
        .ok_or(NetworkError::WouldBlock)
}

/// 消费本物理链路的 ARP。目标资格由当前 FIB 判定，回包沿入接口，不走 IP 路由。
pub(super) fn receive(
    cache: &mut NeighborCache<Ipv4Addr>,
    frame: &[u8],
    tx: ProtocolTxToken<'_>,
    device: &ProtocolDevice,
    now: Instant,
    local: impl Fn(Ipv4Addr, Ipv4Addr) -> bool,
) -> bool {
    let local_mac = device.mac_address();
    let Some((arp, sender_mac, sender, target)) = EthernetFrame::parse(frame)
        .ok()
        .filter(|frame| frame.destination() == local_mac || frame.destination() == BROADCAST)
        .and_then(|frame| ArpPacket::parse(frame.payload()).ok())
        .filter(|arp| arp.hardware_type() == 1 && arp.protocol_type() == 0x0800)
        .and_then(|arp| {
            Some((
                arp,
                <[u8; 6]>::try_from(arp.sender_hardware()).ok()?,
                Ipv4Addr::from(<[u8; 4]>::try_from(arp.sender_protocol()).ok()?),
                Ipv4Addr::from(<[u8; 4]>::try_from(arp.target_protocol()).ok()?),
            ))
        })
    else {
        return false;
    };
    let request = arp.operation() == REQUEST;
    if sender_mac == [0; 6]
        || sender_mac[0] & 1 != 0
        || sender_mac == local_mac
        || !unicast(target)
        || !(unicast(sender) || request && sender.is_unspecified())
        || !local(sender, target)
        || !request && (arp.operation() != REPLY || arp.target_hardware() != local_mac)
    {
        return false;
    }
    let now = now.total_millis().max(0) as u64;
    let learned = !sender.is_unspecified() && cache.learn(sender, sender_mac, now, |_| false);
    if request {
        if learned {
            // 已验收的定向 ARP 请求建立有期限的映射；已失败的同 MAC 路径仍等待回复。
            cache.confirm_existing(sender, sender_mac, now);
        }
        if !send(tx, (local_mac, target), (sender_mac, sender), REPLY) {
            device.record_transmit_drop();
        }
    } else if learned {
        cache.confirm(sender, Some(sender_mac), true, true, false, now);
    }
    learned
}

/// 提交一个地址解析或可达性探测，只有实际 TX 成功才消耗探测次数。
pub(super) fn probe(
    device: &ProtocolDevice,
    local: Ipv4Addr,
    target: Ipv4Addr,
    mac: Option<[u8; 6]>,
) -> bool {
    device.reserve(|_| 0).is_ok_and(|tx| {
        send(
            tx,
            (device.mac_address(), local),
            (mac.unwrap_or(BROADCAST), target),
            REQUEST,
        )
    })
}

fn send(
    tx: ProtocolTxToken<'_>,
    (source, sender): ([u8; 6], Ipv4Addr),
    (destination, target): ([u8; 6], Ipv4Addr),
    operation: u16,
) -> bool {
    let target_hardware = if destination == BROADCAST {
        [0; 6]
    } else {
        destination
    };
    tx.consume_committed(42, ARP, |frame| {
        EthernetHeader {
            source,
            destination,
            type_or_length: ARP,
            tags: &[],
        }
        .emit(frame)
        .expect("exact ARP Ethernet window");
        ArpHeader {
            hardware_type: 1,
            protocol_type: 0x0800,
            operation,
            sender_hardware: &source,
            sender_protocol: &sender.octets(),
            target_hardware: &target_hardware,
            target_protocol: &target.octets(),
        }
        .emit(&mut frame[14..])
        .expect("exact ARP address widths");
        true
    })
    .is_ok()
}

fn unicast(address: Ipv4Addr) -> bool {
    matches!(address.octets()[0], 1..=126 | 128..=223)
}
