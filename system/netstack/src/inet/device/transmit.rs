//! 原生 IP 发送必须观察实际 reservation/commit 结果；标准 Device 的 Option 只用于其既定适配边界。
use super::*;
use core::net::{IpAddr, Ipv6Addr};

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
    /// 取得当前硬件队列的唯一 reservation。
    /// # Returns
    /// 尚未提交的 ticket，未使用时由其 Drop 取消。
    /// # Errors
    /// 硬件原错误原样返回并沿既有设备诊断记录；WouldBlock 不记成硬故障。
    pub(super) fn reserve(&self) -> Result<EthernetTxToken<'_>, NetworkError> {
        let reservation = self
            .device
            .reserve_transmit(0)
            .inspect_err(|error| self.record_transmit_error(*error))?;
        Ok(EthernetTxToken {
            net_id: self.net_id,
            interface_index: self.interface_index,
            reservation: Some(reservation),
            checksum: false,
            maximum_frame_length: self.mtu + 14,
            dropped_transmits: &self.dropped_transmits,
            transmitted_errors: &self.transmitted_errors,
            pending_error: &self.pending_error,
        })
    }
}
impl ProtocolDevice {
    /// 取得真实发送 reservation；硬件错误仍归原设备诊断 owner。
    /// # Parameters
    /// `timestamp` 为本轮冻结时刻，不重新读取时间。
    /// # Returns
    /// 独占的 NIC/lo TX token，未消费则取消 reservation。
    /// # Errors
    /// 返回实际硬件错误或 WouldBlock，不能归并为编码成功。
    pub(in crate::inet) fn reserve(
        &self,
        timestamp: Instant,
    ) -> Result<ProtocolTxToken<'_>, NetworkError> {
        match self {
            Self::Ethernet(device) => device.reserve().map(ProtocolTxToken::Ethernet),
            Self::Loopback(device) => device
                .transmit_shared(timestamp)
                .map(ProtocolTxToken::Loopback)
                .ok_or(NetworkError::WouldBlock),
        }
    }
    /// 在最终 NIC/lo 窗口提交 IPv4 header 与借用载荷；ARP 和提交背压均不会提前推进调用者游标。
    /// # Parameters
    /// `link` 借用出口的 ARP/ND owner 及接口状态；`header` 已完成 IPv4 编码。
    /// `body` 为长度及最终窗口 emitter，`path` 为实际下一跳/逻辑出口；`now` 为本轮时刻，`offload` 请求标准校验卸载。
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
        body: (usize, impl FnOnce(&mut [u8])),
        path: (IpAddr, u32),
        now: Instant,
        offload: bool,
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
        let token = self.reserve(now)?;
        let mut meta = smoltcp::phy::PacketMeta::default();
        meta.interface_index = Some(path.1);
        meta.tx_checksum = offload;
        token.submit_ip(
            destination.map(|destination| (self.mac_address(), destination)),
            header,
            body,
            meta,
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
        let token = self.reserve(now)?;
        let link = if matches!(self, Self::Ethernet(_)) {
            Some((self.mac_address(), ipv6_neighbor(owner, path.0, now)?))
        } else {
            None
        };
        let mut meta = smoltcp::phy::PacketMeta::default();
        meta.interface_index = Some(path.1);
        meta.tx_checksum = offload;
        token.submit_ip(link, header, body, meta)
    }
}
impl ProtocolTxToken<'_> {
    fn submit_ip(
        mut self,
        link: Option<([u8; 6], [u8; 6])>,
        header: &[u8],
        body: (usize, impl FnOnce(&mut [u8])),
        meta: smoltcp::phy::PacketMeta,
    ) -> Result<(), NetworkError> {
        self.set_meta(meta);
        let protocol = if header[0] >> 4 == 4 { 0x0800 } else { 0x86dd };
        let offset = if link.is_some() { 14 } else { 0 };
        let (_, error) =
            self.consume_committed(offset + header.len() + body.0, protocol, |frame| {
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
                frame[offset..offset + header.len()].copy_from_slice(header);
                body.1(&mut frame[offset + header.len()..]);
            });
        error.map_or(Ok(()), Err)
    }
}
