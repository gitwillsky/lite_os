//! IPv4 ioctl、广播权限和实际 ingress 共用 native 地址记录，不从 mask 重建显式广播。
use super::SourceFile;
fn source(sources: &[SourceFile], path: &str) -> String {
    sources
        .iter()
        .find(|source| source.relative == path)
        .expect("IPv4 address source")
        .text
        .split_whitespace()
        .collect()
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let iface = source(sources, "kernel/src/socket/inet/interface.rs");
    let fib = source(sources, "kernel/src/socket/inet/route.rs");
    let config = source(sources, "kernel/src/socket/inet/configuration.rs");
    let ioctl = source(sources, "kernel/src/syscall/socket/interface.rs");
    let raw = source(sources, "kernel/src/socket/inet/raw.rs");
    let udp = source(sources, "kernel/src/socket/inet/udp.rs");
    if !config.contains("broadcast:interface.state.broadcast")
        || !config.contains("ifinterface.state.address==next{returnOk(());}")
        || !config.contains("first<240{returnErr(SocketError::Invalid);}")
        || !config.contains("state.broadcast=address;")
        || !config.contains("u32::from(state.broadcast)==(u32::from(address)|!old_mask)")
        || !iface.contains("self.interface.set_ipv4_broadcast_address(")
        || !fib.contains("append(state.broadcast,32,Ipv4RouteKind::Broadcast)?")
        || !udp.contains("route.kind==super::route::Ipv4RouteKind::Broadcast")
        || !raw.contains("route.kind==super::route::Ipv4RouteKind::Broadcast")
        || !ioctl.contains("encode_inet_sockaddr(&mutifreq,snapshot.broadcast)")
        || ioctl.contains("snapshot.address.unwrap_or(")
        || ioctl.contains("broadcast(address,snapshot.prefix_length)")
    {
        errors.push("ipv4-address: native default prefix, explicit broadcast preservation, no-address error and actual protocol admission must use one address record".into());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_address_record_satisfies_contract() {
        let sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_broadcast_noop_reconstruction_and_protocol_disconnect() {
        for (path, old, new) in [
            (
                "kernel/src/socket/inet/configuration.rs",
                "state.broadcast = address;",
                "let _ = address;",
            ),
            (
                "kernel/src/syscall/socket/interface.rs",
                "snapshot.broadcast",
                "core::net::Ipv4Addr::BROADCAST",
            ),
            (
                "kernel/src/socket/inet/udp.rs",
                "route.kind == super::route::Ipv4RouteKind::Broadcast",
                "peer.address.is_broadcast()",
            ),
        ] {
            let mut sources = super::super::load_sources(&super::super::repository_root()).unwrap();
            let source = sources
                .iter_mut()
                .find(|source| source.relative == path)
                .unwrap();
            assert!(source.text.contains(old));
            source.text = source.text.replace(old, new);
            source.syntax = syn::parse_file(&source.text).unwrap();
            let mut errors = Vec::new();
            check(&sources, &mut errors);
            assert!(
                errors
                    .iter()
                    .any(|error| error.starts_with("ipv4-address:")),
                "{errors:?}"
            );
        }
    }
}
