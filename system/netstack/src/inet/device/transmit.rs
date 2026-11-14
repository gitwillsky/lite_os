//! 原生 IP 发送使用最终窗口与真实提交结果。
use super::*;
use ::tcp::time::Instant;
use core::net::{IpAddr, Ipv6Addr};

/// 原 RX 载荷及其必须重新编码的前缀，供物理出口保留不变的后缀。
pub(in crate::inet) struct ReceivedPayload<'a, 'frame> {
    /// 保有原始 RX 槽的帧票据。
    pub(in crate::inet) frame: &'a NetworkReceiveFrame<'frame>,
    /// 本次发送所引用的完整原始载荷，必须位于 `frame` 内。
    pub(in crate::inet) bytes: &'a [u8],
    /// 原始载荷中最后一个可能被改写的字节之后的位置。
    pub(in crate::inet) render_prefix: usize,
}

fn ipv6_neighbor(
    owner: &mut Option<crate::inet::ipv6::InterfaceAddresses>,
    address: Ipv6Addr,
    now: Instant,
) -> Result<[u8; 6], NetworkError> {
    owner
        .as_mut()
        .ok_or(NetworkError::InvalidConfiguration)?
        .resolve(address, now.total_millis().max(0) as u64)
        .map_err(|error| {
            if error == network_protocol::SocketError::NoMemory {
                NetworkError::NoMemory
            } else {
                NetworkError::InvalidConfiguration
            }
        })?
        .ok_or(NetworkError::WouldBlock)
}
impl EthernetDevice {
    /// 按调用者的亲和策略取得 reservation；越界队列号按活动队列数映射。
    /// # Returns
    /// 尚未提交的 ticket，未使用时由其 Drop 取消。
    /// # Errors
    /// 硬件原错误原样返回并沿既有设备诊断记录；WouldBlock 不记成硬故障。
    pub(super) fn reserve(
        &self,
        select: impl FnOnce(u16) -> u16,
    ) -> Result<EthernetTxToken<'_>, NetworkError> {
        let reservation = self
            .reserve_frame(select)
            .inspect_err(|error| self.record_transmit_error(*error))?;
        Ok(self.transmit_token(Some(reservation)))
    }

    /// 取得物理或 VLAN 发送槽。调用方记录队列背压及设备错误。
    pub(super) fn reserve_frame(
        &self,
        select: impl FnOnce(u16) -> u16,
    ) -> Result<EthernetReservation<'_>, NetworkError> {
        Ok(match &self.link {
            EthernetLink::Physical(nic) => {
                let count = nic.queue_count();
                let queue = select(count);
                let queue = if queue < count { queue } else { queue % count };
                EthernetReservation::Physical(nic.reserve_transmit(queue)?)
            }
            EthernetLink::Vlan(link) => {
                EthernetReservation::Vlan(link.reserve().ok_or(NetworkError::WouldBlock)?)
            }
        })
    }

    pub(super) fn transmit_token<'a>(
        &'a self,
        reservation: Option<EthernetReservation<'a>>,
    ) -> EthernetTxToken<'a> {
        EthernetTxToken {
            net_id: self.net_id,
            queues: &self.queues,
            packets: &self.packets,
            interface_index: self.interface_index,
            reservation,
            checksum: false,
            maximum_frame_length: self.mtu + 14,
            transmitted_errors: &self.transmitted_errors,
            pending_error: &self.pending_error,
        }
    }
}
impl ProtocolDevice {
    /// 只有物理出口声明硬件传输层校验和卸载。
    pub(in crate::inet) fn transmit_checksum(&self) -> bool {
        matches!(self, Self::Ethernet(device) if device.nic().is_some_and(|nic| nic.transmit_capabilities().checksum))
    }

    /// 当前发送路径的活动队列数。逻辑接口与回环各使用自己的单队列。
    pub(in crate::inet) fn transmit_queue_count(&self) -> u16 {
        match self {
            Self::Ethernet(device) => device.nic().map_or(1, NetworkDevice::queue_count),
            Self::Loopback(_) => 1,
        }
    }

    /// 取得真实发送 reservation；硬件错误仍归原设备诊断 owner。
    /// # Parameters
    /// `select` 在物理出口以活动队列数选择发送队列。
    /// # Returns
    /// 独占的 NIC/lo TX token，未消费则取消 reservation。
    /// # Errors
    /// 返回实际硬件错误或 WouldBlock，不能归并为编码成功。
    pub(in crate::inet) fn reserve(
        &self,
        select: impl FnOnce(u16) -> u16,
    ) -> Result<ProtocolTxToken<'_>, NetworkError> {
        match self {
            Self::Ethernet(device) => device.reserve(select).map(ProtocolTxToken::Ethernet),
            Self::Loopback(device) => device
                .reserve()
                .map(ProtocolTxToken::Loopback)
                .ok_or(NetworkError::WouldBlock),
        }
    }
    /// 在最终 NIC/lo 窗口提交 IPv4 header 与借用载荷；ARP 和提交背压均不会提前推进调用者游标。
    /// # Parameters
    /// `link` 借用出口的 ARP/ND owner 及接口状态；`header` 已完成 IPv4 编码。
    /// `body` 为长度及最终窗口 emitter，`path` 为实际下一跳/逻辑出口；`now` 为本轮时刻，末项为校验卸载和队列亲和。
    /// # Returns
    /// 只有实际提交成功才返回 unit，调用者此时才能推进游标。
    /// # Errors
    /// 非法 framing/超 MTU、未解析邻居、reservation 或真实提交错误；失败不消费调用者载荷。
    pub(in crate::inet) fn transmit_ipv4(
        &self,
        link: (
            &mut crate::inet::neighbor::NeighborCache<core::net::Ipv4Addr>,
            crate::inet::InterfaceState,
            &mut Option<crate::inet::ipv6::InterfaceAddresses>,
        ),
        header: &[u8],
        body: (
            usize,
            impl FnOnce(&mut [u8]),
            Option<ReceivedPayload<'_, '_>>,
        ),
        path: (IpAddr, u32),
        now: Instant,
        (offload, select): (bool, impl FnOnce(u16) -> u16),
    ) -> Result<(), NetworkError> {
        if header.len() < 20
            || header.len() > 60
            || header[0] >> 4 != 4
            || usize::from(header[0] & 15) * 4 != header.len()
            || usize::from(u16::from_be_bytes([header[2], header[3]])) != header.len() + body.0
        {
            return Err(NetworkError::InvalidConfiguration);
        }
        if header.len() + body.0 > self.ip_mtu() {
            return Err(NetworkError::FrameTooLarge);
        }
        let ethernet = matches!(self, Self::Ethernet(_));
        let destination = if ethernet {
            let destination = core::net::Ipv4Addr::from(
                <[u8; 4]>::try_from(&header[16..20]).expect("validated IP header"),
            );
            Some(match path.0 {
                IpAddr::V4(address) => {
                    crate::inet::arp::resolve(link.0, link.1, destination, address, now)?
                }
                IpAddr::V6(address) => ipv6_neighbor(link.2, address, now)?,
            })
        } else {
            None
        };
        let token = self.reserve(select)?;
        token.submit_ip(
            destination.map(|destination| (self.mac_address(), destination)),
            header,
            body,
            (path.1, offload),
        )
    }
}
impl ProtocolDevice {
    /// IPv6 使用同一真实提交机制；ND 未完成仅归还 reservation。
    /// # Parameters
    /// `owner` 是唯一 ND 状态；`header` 为完整固定头/前缀，`body` 是长度与 emitter。
    /// `path` 为实际下一跳/逻辑出口；`now` 为本轮时刻，`offload` 请求标准校验卸载。
    /// # Returns
    /// 实际提交成功返回 unit，载荷从不由设备留存。
    /// # Errors
    /// 非法头/超 MTU、ND 失败或未完成、reservation/提交原始错误。
    pub(in crate::inet) fn transmit_ipv6(
        &self,
        owner: &mut Option<crate::inet::ipv6::InterfaceAddresses>,
        header: &[u8],
        body: (usize, impl FnOnce(&mut [u8])),
        path: (core::net::Ipv6Addr, u32),
        now: Instant,
        offload: bool,
    ) -> Result<(), NetworkError> {
        if header.len() < 40
            || header[0] >> 4 != 6
            || header.len().checked_add(body.0).is_none_or(|length| {
                length - 40 != usize::from(u16::from_be_bytes([header[4], header[5]]))
            })
        {
            return Err(NetworkError::InvalidConfiguration);
        }
        let mtu = self.ip_mtu();
        if header.len() + body.0 > owner.as_ref().map_or(mtu, |owner| owner.link_mtu(mtu)) {
            return Err(NetworkError::FrameTooLarge);
        }
        let token = self.reserve(|_| 0)?;
        let link = if matches!(self, Self::Ethernet(_)) {
            Some((self.mac_address(), ipv6_neighbor(owner, path.0, now)?))
        } else {
            None
        };
        token.submit_ip(link, header, (body.0, body.1, None), (path.1, offload))
    }
}
impl ProtocolTxToken<'_> {
    /// 编码并提交 IP 帧。若 `original` 提供原始载荷及改写前缀长度，
    /// `render_body` 可只收到该前缀。后缀由独立的物理出口线程从原 RX 槽复制。
    /// # Errors
    /// 载荷票据、帧边界或实际设备提交失败时返回原错误。
    pub(in crate::inet) fn submit_ip(
        mut self,
        link: Option<([u8; 6], [u8; 6])>,
        header: &[u8],
        (body_length, render_body, original): (
            usize,
            impl FnOnce(&mut [u8]),
            Option<ReceivedPayload<'_, '_>>,
        ),
        (interface, offload): (u32, bool),
    ) -> Result<(), NetworkError> {
        self.set_delivery(interface, offload);
        let protocol = if header[0] >> 4 == 4 { 0x0800 } else { 0x86dd };
        let offset = if link.is_some() { 14 } else { 0 };
        let prefix = offset + header.len();
        let length = prefix + body_length;
        let render_header = |frame: &mut [u8]| {
            if let Some((source, destination)) = link {
                packet_wire::ethernet::EthernetHeader {
                    source,
                    destination,
                    type_or_length: protocol,
                    tags: &[],
                }
                .emit(frame)
                .expect("exact IP Ethernet window");
            }
            frame[offset..prefix].copy_from_slice(header);
        };
        if let Some(ReceivedPayload {
            frame,
            bytes,
            render_prefix,
        }) = original
            && bytes.len() == body_length
            && render_prefix < body_length
            && link.is_some()
            && !offload
            && let Self::Ethernet(token) = &mut self
            && let Some(EthernetReservation::Physical(reservation)) = &token.reservation
            && length <= token.maximum_frame_length
            && length <= reservation.capacity()
            && token.queues.separate_transmit_worker(reservation.queue())
            && !token.packets.has_endpoints()
            && let Some(payload) = frame.retain_payload(&bytes[render_prefix..])?
        {
            let Some(EthernetReservation::Physical(mut reservation)) = token.reservation.take()
            else {
                unreachable!("physical TX reservation checked above");
            };
            let ready = prefix + render_prefix;
            let output = &mut reservation.buffer_mut()[..ready];
            render_header(output);
            if render_prefix != 0 {
                render_body(&mut output[prefix..]);
            }
            let result = reservation.submit_payload(ready, payload);
            token.record_transmit_error(result.as_ref().err().copied());
            return result;
        }
        self.consume_committed(length, protocol, |frame| {
            render_header(frame);
            render_body(&mut frame[prefix..]);
            true
        })
    }
}
