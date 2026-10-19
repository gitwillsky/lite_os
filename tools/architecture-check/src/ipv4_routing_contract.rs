//! IPv4 FIB/source/egress/LOCAL iif/reply/proc共享唯一NET owner，拒绝默认网关双轨。
use super::SourceFile;
fn source(sources: &[SourceFile], path: &str) -> String {
    sources
        .iter()
        .find(|source| source.relative == path)
        .expect("IPv4 owner source")
        .text
        .split_whitespace()
        .collect()
}
pub(super) fn check(root: &std::path::Path, sources: &[SourceFile], errors: &mut Vec<String>) {
    let inet = source(sources, "kernel/src/socket/inet.rs");
    let config = source(sources, "kernel/src/socket/inet/configuration.rs");
    let interface = source(sources, "kernel/src/socket/inet/interface.rs");
    let fib = source(sources, "kernel/src/socket/inet/route.rs");
    let poll = source(sources, "kernel/src/socket/inet/poll.rs");
    let timing = source(sources, "kernel/src/socket/inet/timing.rs");
    let replies = source(sources, "kernel/src/socket/inet/route_reply.rs");
    let udp = source(sources, "kernel/src/socket/inet/udp.rs");
    let raw = source(sources, "kernel/src/socket/inet/raw.rs");
    let tcp = source(sources, "kernel/src/socket/inet/tcp.rs");
    let loopback = source(sources, "kernel/src/socket/inet/device/loopback.rs");
    let ioctl = source(sources, "kernel/src/syscall/socket/interface.rs");
    let opened = source(sources, "kernel/src/fs/procfs/opened_operations.rs");
    let formatter = source(sources, "kernel/src/fs/procfs/networking.rs");
    let mechanism = std::fs::read_to_string(root.join("vendor/smoltcp/src/iface/interface/mod.rs"))
        .unwrap()
        .split_whitespace()
        .collect::<String>();
    let tcp_ingress =
        std::fs::read_to_string(root.join("vendor/smoltcp/src/iface/interface/tcp.rs"))
            .unwrap()
            .split_whitespace()
            .collect::<String>();
    let state = inet
        .split("structInterfaceState{")
        .nth(1)
        .unwrap_or("")
        .split("structEndpointState{")
        .next()
        .unwrap_or("");
    if !inet.contains("routing:route::RoutingTable")
        || !inet.contains("egress_routes:Vec<Option<route::RouteSelection>>")
        || state.contains("gateway:")
        || config.contains("fnconfigure_gateway(")
        || interface.contains("select_interface_index")
        || interface.contains("routes_mut()")
        || !interface.contains("self.routing.select(destination,bound,&self.interfaces)")
        || !fib.contains("prefixes:[FallibleMap<Key,Record>;33]")
        || !fib.contains("used_prefixes:u64")
        || !fib.contains("fortablein[TABLE_LOCAL,TABLE_MAIN]")
        || !fib.contains("prefixes&=!(1u64<<prefix)")
        || !fib.contains("request.scope>=minimum_scope")
        || !fib.contains("if!create&&first.is_none()")
    {
        errors.push("ipv4-fib: actual NET must own one indexed FIB with native table/prefix/metric/alias and scope admission, without per-interface gateway mirrors".into());
    }
    let prepare = config.find("letprepared=RoutingTable::prepare_interface(index,next)?;");
    let publish = config.find("self.interface_mut(index)?.state=next;");
    if prepare.is_none()
        || publish.is_none()
        || prepare >= publish
        || !config.contains("self.routing.device_down(index,true)")
        || !config.contains("self.routing.device_down(index,false)")
        || !config.contains("self.routing.replace_interface(index,prepared)")
        || !fib.contains("!force&&record.request.scope==254")
    {
        errors.push("ipv4-fib: prepare all address routes before publication; OOM preserves actual state and DOWN preserves HOST LOCAL without reviving retired static routes".into());
    }
    if !udp.contains("network.source_address(route)")
        || !raw.contains("network.source_address(route)")
        || !tcp.contains("network.source_address(route)")
        || !poll.contains("self.routing.select(destination,bound,&self.interfaces).ok()")
        || !poll.contains(
            "context.set_egress_route(ipv4(route.next_hop),route.logical_interface_index,",
        )
        || !poll.contains("meta.interface_index.unwrap_or(current.index)")
        || !poll.contains("routing.ingress(source,destination,index,")
        || !loopback.contains("PacketBuffer<'static,Delivery>")
        || !loopback.contains("meta.interface_index=")
        || !mechanism.contains("permitted(item.meta.handle,&mutitem.socket,&mutself.inner)")
        || !mechanism.contains("self.inner.egress_route=None;")
        || !mechanism.contains("raw_socket.accepts_interface(self.interface_index())")
        || !fib.contains("self.select_facts(source,Some(index),interfaces.clone())")
        || !tcp_ingress.contains("self.reply_bound_interface=tcp_socket.bound_interface();")
        || !mechanism.contains("facts.bound_reverse_route")
        || !fib.contains("metrics:Ipv4RouteMetrics")
        || !fib.contains("transport:record.request.metrics.transport(output_interface.mtu)")
        || !poll.contains("tcp::maintain_clocks(self,timestamp)")
        || !timing.contains("tcp.maintenance_deadline()")
        || !timing.contains("network.plan_egress_routes();")
        || !timing.contains(".poll_at_filtered(timestamp,sockets,")
    {
        errors.push("ipv4-fib: source, transport dispatch and actual LOCAL logical iif must consume the same FIB facts before queue/neighbor mutation".into());
    }
    if !replies.contains("REPLY_BYTES:usize=256*1024")
        || !replies.contains("REPLY_PACKETS:usize=64")
        || !replies.contains("packet.capacity().checked_add(VacantEntry")
        || !replies.contains(
            "self.routing.select(packet.dst_addr(),reply.bound_interface,&self.interfaces,)",
        )
        || !replies.contains("interface.interface.dispatch_ipv4_packet(")
        || !poll.contains("reply_drops+=1;")
        || !opened.contains("source.route_snapshot(target)?")
        || !formatter.contains("forrouteinroutes{")
        || formatter.contains("interface.gateway")
        || !ioctl.contains("configure_ipv4_route(scope,request,operation==SIOCDELRT,")
        || !ioctl.contains("(i32::from(metric)-1)asu32")
    {
        errors.push("ipv4-fib: standard route codec/proc must project actual records; bounded IP replies use the same FIB rather than falling back to ingress NIC on congestion/OOM".into());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_ipv4_owner_satisfies_contract() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).unwrap();
        let mut errors = Vec::new();
        check(&root, &sources, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_route_mirror_half_publication_wrong_local_iif_and_ingress_reply_fallback() {
        for (path, old, new) in [
            (
                "kernel/src/socket/inet/interface.rs",
                ".select(destination, bound, &self.interfaces)",
                ".select(Ipv4Addr::UNSPECIFIED, bound, &self.interfaces)",
            ),
            (
                "kernel/src/socket/inet/configuration.rs",
                "let prepared = RoutingTable::prepare_interface(index, next)?;",
                "self.interface_mut(index)?.state = next;\n        let prepared = RoutingTable::prepare_interface(index, next)?;",
            ),
            (
                "kernel/src/socket/inet/poll.rs",
                "meta.interface_index.unwrap_or(current.index)",
                "current.index",
            ),
            (
                "kernel/src/socket/inet/route_reply.rs",
                "reply.bound_interface,",
                "Some(reply.origin),",
            ),
            (
                "kernel/src/fs/procfs/opened_operations.rs",
                "source.route_snapshot(target)?",
                "source.route_snapshot(&crate::socket::initial_net())?",
            ),
        ] {
            let root = super::super::repository_root();
            let mut sources = super::super::load_sources(&root).unwrap();
            let source = sources
                .iter_mut()
                .find(|source| source.relative == path)
                .unwrap();
            assert!(source.text.contains(old), "{path}: {old}");
            source.text = source.text.replace(old, new);
            source.syntax = syn::parse_file(&source.text).unwrap();
            let mut errors = Vec::new();
            check(&root, &sources, &mut errors);
            assert!(
                errors.iter().any(|error| error.starts_with("ipv4-fib:")),
                "{errors:?}"
            );
        }
    }
}
