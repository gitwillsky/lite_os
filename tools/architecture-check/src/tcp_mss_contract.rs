//! TCP_MAXSEG 必须约束实际 SYN/数据/Nagle/Reno；getter 不能用 user policy 冒充协商 cache。
use super::SourceFile;
use std::path::Path;

const VENDOR: &str = "vendor/smoltcp/src/socket/tcp.rs";
fn normalized(source: &str) -> String {
    source.split_whitespace().collect()
}
fn source<'a>(sources: &'a [SourceFile], path: &str) -> &'a str {
    &sources
        .iter()
        .find(|file| file.relative == path)
        .expect("TCP MSS source")
        .text
}
fn before(source: &str, first: &str, second: &str) -> bool {
    matches!((source.find(first), source.find(second)), (Some(a), Some(b)) if a < b)
}
fn check_transport(source: &str, errors: &mut Vec<String>) {
    let source = normalized(source);
    let setter = source
        .split("pubfnset_user_mss(")
        .nth(1)
        .unwrap_or("")
        .split("pubfnmax_segment_size(")
        .next()
        .unwrap_or("");
    if source.matches("self.capture_mss(").count() != 2
        || !source.contains("self.capture_mss(cx,local_endpoint.addr)")
        || !source.contains("self.capture_mss(cx,ip_repr.dst_addr())")
        || !source.contains(
            "letmss=self.effective_mss();self.congestion_controller.inner_mut().set_mss(mss)",
        )
        || !source.contains("leteffective_mss=local_mss.min(self.effective_mss())")
        || !source.contains(".min(self.effective_mss()).min(cx.ip_mtu()")
        || !source.contains("letmutmax_segment_size=self.advertised_mss.expect(")
        || !source.contains("route.advertised_mss")
        || !source.contains(".min(self.sender_mss_limit)")
        || !source.contains("self.route_send_mss=route_mss;self.update_send_mss()")
        || !source.contains("self.receive_window=route.map_or(")
        || !source.contains("self.rx_buffer.window().min(self.receive_window)")
        || !source.contains("self.rtte.initialize_route(self.route_initial_rtt)")
        || !source.contains("self.send_mss=self.remote_mss.min(self.route_send_mss)")
        || setter.contains("send_mss=")
        || setter.contains("advertised_mss=")
    {
        errors.push("tcp-mss: actual active-open/SYN must capture separate actual advertisement/PMTU cache for wire, segmentation, Nagle and Reno; setter must preserve existing negotiation".into());
    }
}
fn check_kernel(sources: &[SourceFile], errors: &mut Vec<String>) {
    let options = normalized(source(sources, "kernel/src/net/inet/options.rs"));
    let tcp = normalized(source(sources, "kernel/src/net/inet/tcp.rs"));
    let lifecycle = normalized(source(sources, "kernel/src/net/inet/tcp/lifecycle.rs"));
    let abi = normalized(source(sources, "kernel/src/syscall/socket/options.rs"));
    if !options.contains("socket.set_user_mss(self.user_mss)")
        || !options.contains("value!=0&&!(88..=32767).contains(&value)")
        || !tcp.contains("ifletTcpMode::Listening{send_mss,..}=state.mode")
        || !tcp.contains(".max_segment_size()asi32")
        || !lifecycle.contains("state.options.user_mss=None")
        || !before(
            &lifecycle,
            "tcp.set_user_mss(None)",
            "*tcp=placeholder_socket()",
        )
        || !lifecycle.contains("*tcp=placeholder_socket();tcp.set_send_mss(send_mss)")
        || !lifecycle.contains("prepared.set_send_mss(send_mss)")
        || !abi.contains("(IPPROTO_TCP,TCP_MAXSEG)=>read_i32(value,length)")
        || !abi.contains("(IPPROTO_TCP,TCP_MAXSEG)=>InternetOption::MaxSegment")
    {
        errors.push("tcp-mss: native range/getter and listener cache must remain in real protocol owner; disconnect must clear user_mss and transfer sender cache".into());
    }
}
pub(super) fn check(root: &Path, sources: &[SourceFile], errors: &mut Vec<String>) {
    match std::fs::read_to_string(root.join(VENDOR)) {
        Ok(source) => check_transport(&source, errors),
        Err(error) => errors.push(format!("{VENDOR}: {error}")),
    }
    check_kernel(sources, errors);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Vec<SourceFile> {
        super::super::load_sources(&super::super::repository_root()).unwrap()
    }
    #[test]
    fn actual_transport_and_native_mss_owner_satisfy_contract() {
        let mut errors = Vec::new();
        check(&super::super::repository_root(), &fixture(), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    fn reject_transport(old: &str, new: &str) {
        let source = std::fs::read_to_string(super::super::repository_root().join(VENDOR)).unwrap();
        assert!(source.contains(old));
        let source = source.replace(old, new);
        syn::parse_file(&source).unwrap();
        let mut errors = Vec::new();
        check_transport(&source, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("actual active-open/SYN")),
            "{errors:?}"
        );
    }
    #[test]
    fn rejects_negotiating_at_accept_instead_of_syn() {
        reject_transport(
            "self.capture_mss(cx, ip_repr.dst_addr());",
            "self.ignore_mss(cx, ip_repr.dst_addr());",
        );
    }
    #[test]
    fn rejects_ignoring_mss_on_actual_data_path() {
        reject_transport(".min(self.effective_mss())", ".min(self.remote_mss)");
    }
    #[test]
    fn rejects_using_receive_advertisement_as_sender_mtu() {
        reject_transport(
            "self.remote_mss.min(self.route_send_mss)",
            "self.remote_mss.min(self.advertised_mss.unwrap_or(usize::MAX))",
        );
    }
    #[test]
    fn rejects_ignoring_actual_route_receive_window() {
        reject_transport(
            "self.rx_buffer.window().min(self.receive_window)",
            "self.rx_buffer.window()",
        );
    }
    #[test]
    fn rejects_forging_new_cache_on_established_setter() {
        reject_transport(
            "self.user_mss = value;",
            "self.user_mss = value; self.send_mss = 88;",
        );
    }
    #[test]
    fn rejects_retaining_user_cap_across_disconnect() {
        let mut sources = fixture();
        let file = sources
            .iter_mut()
            .find(|file| file.relative == "kernel/src/net/inet/tcp/lifecycle.rs")
            .unwrap();
        assert!(file.text.contains("state.options.user_mss = None;"));
        file.text = file.text.replace(
            "state.options.user_mss = None;",
            "state.options.user_mss = Some(88);",
        );
        file.syntax = syn::parse_file(&file.text).unwrap();
        let mut errors = Vec::new();
        check_kernel(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("disconnect must clear")),
            "{errors:?}"
        );
    }
}
