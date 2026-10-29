//! 已实现的标准 INET 选项必须读取唯一 protocol policy，并应用于所有实际 transport 生命周期。
use super::SourceFile;
fn source<'a>(sources: &'a [SourceFile], path: &str) -> &'a str {
    &sources
        .iter()
        .find(|source| source.relative == path)
        .expect("INET option source")
        .text
}
fn normalized(text: &str) -> String {
    text.split_whitespace().collect()
}
fn before(text: &str, first: &str, second: &str) -> bool {
    matches!((text.find(first),text.find(second)),(Some(a),Some(b)) if a < b)
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let options = normalized(source(sources, "kernel/src/net/inet/options.rs"));
    let tcp = normalized(source(sources, "kernel/src/net/inet/tcp.rs"));
    let lifecycle = normalized(source(sources, "kernel/src/net/inet/tcp/lifecycle.rs"));
    let accept = normalized(source(sources, "kernel/src/net/inet/tcp/accept.rs"));
    let io = normalized(source(sources, "kernel/src/net/inet/tcp/io.rs"));
    let raw = normalized(source(sources, "kernel/src/net/inet/raw.rs"));
    let abi = normalized(source(sources, "kernel/src/syscall/socket/options.rs"));
    let udp = normalized(source(sources, "kernel/src/net/inet/udp.rs"));
    let control = normalized(source(sources, "kernel/src/syscall/socket/control.rs"));
    let message = normalized(source(sources, "kernel/src/syscall/socket/message.rs"));
    let socket = normalized(source(sources, "kernel/src/net/mod.rs"));
    let update = tcp
        .split("fnupdate_transport_options(")
        .nth(1)
        .unwrap_or("")
        .split("#[cfg(test)]")
        .next()
        .unwrap_or("");
    if !options.contains("socket.set_nagle_enabled(!self.no_delay)")
        || !options.contains("socket.set_hop_limit(self.hop_limit)")
        || !before(
            update,
            "update(&mutstate.options)",
            "forentryin&state.handles",
        )
        || !update.contains("ifentry.passive.is_none()")
        || !update.contains("state.options.apply_tcp(")
        || !before(
            &tcp,
            "options.apply_tcp(network.sockets.get_mut",
            ".listen(endpoint)",
        )
        || !lifecycle.contains("options.apply_tcp(network.sockets.get_mut")
        || !before(&accept, "listener_options.apply_tcp(", ".listen(endpoint)")
        || !io.contains("options:state.options")
        || !io.contains("state.options.apply_tcp(tcp)")
    {
        errors.push("inet-options: one TCP policy must initialize every real handle and preserve frozen passive children".into());
    }
    if !raw.contains("options:super::InetSocketOptions")
        || raw.contains("hop_limit:u8")
        || raw.contains("device_bound:bool")
        || !raw.contains("letoptions=state.options;")
        || !raw.contains("options.hop_limit")
        || !raw.contains("try_from(&packet[16..20])")
        || !options.contains("options.packet_info")
        || options.contains("current_task(")
    {
        errors.push("inet-options: raw/UDP/TCP options must remain in the actual protocol owner and raw PKTINFO must use the real packet destination".into());
    }
    if control.contains("constINTERFACE_INDEX")
        || !control.contains("info.interface_indexasi32")
        || !control.contains("info.specific_address.octets()")
        || !control.contains("info.destination.octets()")
        || !control.contains("info.interface_index=Some(index)")
        || !control.contains("info.source_address=(!source.is_unspecified()).then_some(source)")
        || !message.contains("packet_info:ancillary.packet_info")
        || !message.contains("internet_packet_info:received.internet_packet_info")
        || !socket.contains("socket.send_to(input,Some(address),packet_info)")
        || !udp.contains("metadata.local_spec_address")
        || !udp.contains("metadata.interface_index")
        || !udp.contains("metadata.interface_index=Some(*interface_index)")
        || !udp.contains("requested_source.map(ipv4).or_else(||route_address.map(ipv4))")
        || !raw.contains("socket.receive_metadata()")
        || !raw.contains("specific_address:from_ip(metadata.local_spec_address")
        || !raw.contains(".set_local_address(")
        || raw.contains("bound_address:")
    {
        errors.push("inet-options: PKTINFO must flow from actual queued ingress metadata and the single-request source/oif route; raw binding must have one transport owner".into());
    }
    if !abi.contains("fnread_ip_i32(")
        || !abi.contains("iflength==0{returnOk(0);}")
        || !abi.contains("ifvalue==-1{None}")
        || !abi.contains("ifcapacity<IFNAMSIZasi32{return-errno::EINVAL;}")
        || !abi.contains("iflevel==IPPROTO_IP&&capacity>0&&capacity<4{1}")
        || !abi.contains("iflevel!=SOL_SOCKET{")
        || !abi.contains("letcount=length.min(IFNAMSIZ-1);")
        || !abi.contains("letlength=lengthasu32asi32;iflength<0{return-errno::EINVAL;}")
        || !options.contains("ifoptions.bound_to_device.is_some()&&!authority.capable(")
        || !options.contains("&self.memory.namespace_ref().user_namespace(),syscall_abi::capability::Capability::NET_RAW")
        || !before(
            abi.split("iflevel!=SOL_SOCKET{").nth(1).unwrap_or(""),
            "copy_to_user(length",
            "copy_to_user(value",
        )
    {
        errors.push("inet-options: native short IP values, TTL default, bind-device sizing and protocol optlen-before-value copyout must be retained".into());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Vec<SourceFile> {
        super::super::load_sources(&super::super::repository_root()).unwrap()
    }
    fn reject(path: &str, old: &str, new: &str, expected: &str) {
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
            errors.iter().any(|error| error.contains(expected)),
            "{errors:?}"
        );
    }
    #[test]
    fn actual_option_owner_satisfies_contract() {
        let mut errors = Vec::new();
        check(&fixture(), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_modifying_established_passive_child() {
        reject(
            "kernel/src/net/inet/tcp.rs",
            "if entry.passive.is_none() {",
            "if true {",
            "frozen passive",
        );
    }
    #[test]
    fn rejects_losing_option_on_accept_replacement() {
        reject(
            "kernel/src/net/inet/tcp/accept.rs",
            "listener_options.apply_tcp(",
            "listener_options.ignore_tcp(",
            "every real handle",
        );
    }
    #[test]
    fn rejects_raw_option_mirror() {
        reject(
            "kernel/src/net/inet/raw.rs",
            "pub(super) options: super::InetSocketOptions,",
            "pub(super) hop_limit: u8,",
            "actual protocol owner",
        );
    }
    #[test]
    fn rejects_raw_codec_detached_from_actual_option_policy() {
        reject(
            "kernel/src/net/inet/raw.rs",
            "let options = state.options;",
            "let options = super::InetSocketOptions::default();",
            "actual protocol owner",
        );
        reject(
            "kernel/src/net/inet/raw.rs",
            "options.hop_limit",
            "None::<u8>",
            "actual protocol owner",
        );
    }
    #[test]
    fn rejects_overread_and_bound_socket_rebind_without_owning_net_capability() {
        reject(
            "kernel/src/syscall/socket/options.rs",
            "length.min(IFNAMSIZ - 1)",
            "length.min(IFNAMSIZ)",
            "bind-device sizing",
        );
        reject(
            "kernel/src/net/inet/options.rs",
            "if options.bound_to_device",
            "if false",
            "bind-device sizing",
        );
    }
    #[test]
    fn rejects_silent_nonstandard_device_buffer_truncation() {
        reject(
            "kernel/src/syscall/socket/options.rs",
            "capacity < IFNAMSIZ as i32",
            "capacity < 1",
            "bind-device sizing",
        );
    }
    #[test]
    fn rejects_pktinfo_interface_fabrication_and_lost_send_request() {
        reject(
            "kernel/src/syscall/socket/control.rs",
            "info.interface_index as i32",
            "1i32",
            "queued ingress metadata",
        );
        reject(
            "kernel/src/syscall/socket/message.rs",
            "packet_info: ancillary.packet_info",
            "packet_info: None",
            "single-request source/oif",
        );
        reject(
            "kernel/src/net/inet/udp.rs",
            "metadata.local_spec_address",
            "metadata.local_address",
            "queued ingress metadata",
        );
        reject(
            "kernel/src/net/inet/udp.rs",
            "requested_source\n                    .map(ipv4)\n                    .or_else(|| route_address.map(ipv4))",
            "route_address.map(ipv4).or_else(|| requested_source.map(ipv4))",
            "single-request source/oif",
        );
    }
}
