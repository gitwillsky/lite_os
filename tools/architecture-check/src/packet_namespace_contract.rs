//! AF_PACKET必须捕获实际NET/link；全局目录只持Weak membership，不能持一张默认NIC。
use super::SourceFile;
fn normalized(source: &str) -> String {
    source.split_whitespace().collect()
}
fn source<'a>(sources: &'a [SourceFile], path: &str) -> &'a str {
    &sources
        .iter()
        .find(|source| source.relative == path)
        .expect("packet namespace source")
        .text
}
fn before(source: &str, first: &str, second: &str) -> bool {
    matches!((source.find(first),source.find(second)),(Some(a),Some(b)) if a < b)
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let packet = normalized(source(sources, "kernel/src/socket/packet.rs"));
    let phy = normalized(source(sources, "kernel/src/socket/device.rs"));
    let lo = normalized(source(sources, "kernel/src/socket/device/loopback.rs"));
    let inet = normalized(source(sources, "kernel/src/socket/inet.rs"));
    let poll = normalized(source(sources, "kernel/src/socket/inet/poll.rs"));
    let config = normalized(source(sources, "kernel/src/socket/inet/configuration.rs"));
    let prepare = normalized(source(sources, "kernel/src/socket/preparation.rs"));
    let observe = normalized(source(sources, "kernel/src/socket/observation.rs"));
    let packet_poll = packet
        .split("fntry_poll_state(")
        .nth(1)
        .unwrap_or("")
        .split("fntake_error(")
        .next()
        .unwrap_or("");
    if packet.contains("network_device(") || packet.contains("registry.device") || packet.contains("constINTERFACE_INDEX")
        || packet.contains("initial_net_ref(") || !packet.contains("memory:Arc<super::SocketMemory>")
        || !packet.contains("net_id:memory.namespace_ref().id()")
        || !packet.contains("state.net_id==net_id&&(state.interface_index==0||state.interface_index==source.interface_index)")
        || !packet.contains("super::inet::packet_interface(self.memory.namespace_ref(),")
        || !prepare.contains("PacketSocket::new(kind,protocol,notify,self.memory.clone(),)")
        || !config.contains("letnetwork=stack(scope)?.lock()?")
        || !phy.contains("packet::deliver(self.net_id,self.interface_index,self.mac,frame)")
        || !inet.contains("NetworkInterface::loopback(scope.id(),loopback_up)")
        || !inet.contains("scope.id(),u32::try_from(ordinal)")
        || !lo.contains("crate::socket::packet::deliver_loopback(self.0.net_id,header,packet)")
        || !before(&lo,"deliver_loopback(","operation(ifheader[12..14]")
    {
        errors.push("packet-net: actual OFD NET/link must select send and enqueue before L3; global Weak directory must not retain an initial NIC or fixed interface".into());
    }
    if !before(
        packet_poll,
        "drop(registry)",
        "packet_transmit_available(self.memory.namespace_ref()",
    ) || !observe.contains("SocketBackend::Packet(socket)=>socket.try_poll_state()")
        || !packet.contains("fnpublish_transmit_ready(")
        || !packet.contains(
            "state.net_id==net_id&&(state.interface_index==0||state.interface_index==indexasi32)",
        )
        || !inet.contains("packet::publish_transmit_ready(namespace.id(),index)")
        || !poll.contains("packet_transmit_ready:[Option<u32>;8]")
    {
        errors.push("packet-net: deferred observation must not wait while holding registry and TX edges must remain scoped to actual NET/interface with bounded storage".into());
    }
    let receive = phy
        .split("fnreceive(")
        .last()
        .unwrap_or("")
        .split("fntransmit(")
        .next()
        .unwrap_or("");
    if !prepare.contains("SocketType::Datagram|SocketType::Raw")
        || !packet.contains("link_header:[u8;ETH_HEADER_LENGTH]")
        || !packet.contains("output.append(&packet.link_header)")
        || !packet.contains("sender!=Some(id)&&matches(state,net_id,source)")
        || !packet.contains("ifsender==Some(id)||!matches(state,net_id,source)")
        || !packet.contains("packet_type:PACKET_OUTGOING")
        || !lo.contains("queue:RefCell<PacketBuffer<'static,Delivery>>")
        || !phy.contains("reservation:Option<NetworkTransmit>")
        || receive.contains("Ok(reservation)=>reservation?")
        || !receive.contains("Ok(reservation)=>reservation,")
        || !phy.contains("self.dropped_transmits.set(")
    {
        errors.push("packet-net: RAW/DGRAM must share queued link facts and native outgoing origin exclusion; RX must continue when only TX is congested".into());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Vec<SourceFile> {
        super::super::load_sources(&super::super::repository_root()).unwrap()
    }
    #[test]
    fn actual_packet_domain_and_link_satisfy_contract() {
        let mut errors = Vec::new();
        check(&fixture(), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_cross_domain_delivery_and_unscoped_capacity_edge() {
        for (path, old, new) in [
            (
                "kernel/src/socket/packet.rs",
                "state.net_id == net_id",
                "true",
            ),
            (
                "kernel/src/socket/inet.rs",
                "packet::publish_transmit_ready(namespace.id(), index)",
                "packet::publish_transmit_ready(6, index)",
            ),
            (
                "kernel/src/socket/device.rs",
                "self.net_id, self.interface_index, self.mac, frame",
                "6, 1, self.mac, frame",
            ),
            (
                "kernel/src/socket/packet.rs",
                "drop(registry);",
                "let _registry_is_still_held = &registry;",
            ),
            (
                "kernel/src/socket/device.rs",
                "Ok(reservation) => reservation,",
                "Ok(reservation) => reservation?,",
            ),
            (
                "kernel/src/socket/packet.rs",
                "if sender == Some(id) ||",
                "if false ||",
            ),
        ] {
            let mut sources = fixture();
            let file = sources
                .iter_mut()
                .find(|file| file.relative == path)
                .unwrap();
            assert!(file.text.contains(old));
            file.text = file.text.replace(old, new);
            file.syntax = syn::parse_file(&file.text).unwrap();
            let mut errors = Vec::new();
            check(&sources, &mut errors);
            assert!(
                errors.iter().any(|error| error.starts_with("packet-net:")),
                "{errors:?}"
            );
        }
    }
}
