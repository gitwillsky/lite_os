//! UDP 的关联路径查询；双族输出事实归唯一 IP 路由 owner。
use super::*;

/// 已关联 IPv6 UDP 查询 IP 层当前路径值；未关联无唯一目的，显式发送上限不是路径事实。
pub(in crate::inet) fn path_mtu(
    network: &NetworkStack,
    id: u64,
    ipv4: bool,
) -> Result<i32, SocketError> {
    i32::try_from(connected_output(network, id, ipv4)?.mtu).map_err(|_| SocketError::Invalid)
}

/// 按关联远端解析本次 IP 路径。未关联或没有路径时返回领域错误。
pub(in crate::inet) fn connected_output(
    network: &NetworkStack,
    id: u64,
    ipv4: bool,
) -> Result<super::super::route::Output, SocketError> {
    let state = network
        .udp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    if !ipv4 && !state.is_ipv6() {
        return Err(SocketError::OperationNotSupported);
    }
    let peer = state.peer.ok_or(SocketError::NotConnected)?;
    if ipv4 && !peer.ip().to_canonical().is_ipv4() {
        return Err(SocketError::OperationNotSupported);
    }
    let (source, outgoing) = state.send_packet_info(network, peer, &[])?;
    network.ip_output(
        source,
        wire_address(peer),
        (outgoing, state.options.mark),
        super::super::now(),
    )
}
