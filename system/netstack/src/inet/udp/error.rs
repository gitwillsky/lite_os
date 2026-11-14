//! ICMP 引用反查与错误消费。错误状态只在所属 UDP endpoint 持有。
use super::*;
use packet_wire::udp::UdpPacket;

/// 由原地址与端口匹配发送端点，按实际选项排队并报告硬错误。
pub(in crate::inet) fn receive_icmp_error(
    endpoints: &mut FallibleMap<u64, EndpointState>,
    report: &super::super::icmp_error::Report<'_>,
    accept: impl FnOnce((Option<u32>, u32), u8) -> Option<u32>,
) {
    let ipv6 = report.source.is_ipv6();
    let Ok(udp) = UdpPacket::parse_quote(report.transport, ipv6) else {
        return;
    };
    let length = usize::from(u16::from_be_bytes([
        report.transport[4],
        report.transport[5],
    ]));
    if report.transport_length.is_some_and(|total| length > total) {
        return;
    }
    let packet = Datagram {
        source: InetAddress::new(report.destination, udp.destination_port()),
        destination: InetAddress::new(report.source, udp.source_port()),
        interface: report.interface,
        multicast: false,
        packet_info: None,
        payload: &[],
    };
    let Some(id) = select_endpoint(endpoints, packet) else {
        return;
    };
    let state = endpoints.get_mut(&id).expect("selected UDP");
    let mode = state.options.ipv4_mtu_discovery;
    let Some(info) = accept((state.options.bound_to_device, state.options.mark), mode) else {
        return;
    };
    let (error, payload) = report.record(
        udp.destination_port(),
        info,
        (udp.payload(), report.transport_offset() + 8),
        |address| state.present(address, Some(report.interface)),
    );
    let hard = report.hard && state.peer.is_some() && (ipv6 || !report.too_big() || mode != 0);
    state.errors.report(error, payload, hard);
}

/// 消费 SO_ERROR 的唯一错误槽；关闭或 owner 不可用时不重建状态。
pub(in crate::inet) fn take_error(endpoint: &InetSocket, id: u64) -> Option<SocketError> {
    endpoint
        .stack()
        .ok()?
        .lock()
        .ok()?
        .udp_endpoints
        .get_mut(&id)?
        .errors
        .pending
        .take()
}
