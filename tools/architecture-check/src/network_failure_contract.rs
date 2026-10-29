//! NIC错误属于实际设备，poll必须同时移交同轮其他设备的edges/backlog。
use super::SourceFile;

fn source(sources: &[SourceFile], path: &str) -> String {
    sources
        .iter()
        .find(|file| file.relative == path)
        .expect("network failure source")
        .text
        .split_whitespace()
        .collect()
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let inet = source(sources, "kernel/src/net/inet.rs");
    let poll = source(sources, "kernel/src/net/inet/poll.rs");
    let readiness = source(sources, "kernel/src/net/inet/readiness.rs");
    let device = source(sources, "kernel/src/net/inet/device.rs");
    let proc = source(sources, "kernel/src/fs/procfs/networking.rs");
    if inet.contains("take_device_error(")
        || readiness.contains("pending_device_error(")
        || inet.contains("ifletOk(poll)=poll")
        || poll.contains("returnErr(error)")
        || !poll.contains(
            "pub(super)device_failures:[Option<(u32,crate::devices::model::network::NetworkError)>;8]",
        )
        || !poll.contains("interface.device.take_error()")
        || !inet.contains("forindexinpoll.packet_transmit_ready.into_iter().flatten()")
        || !inet.contains("backlog|=poll.backlog||notification_backlog||cleanup_backlog")
        || !device
            .contains("self.received_errors.set(self.received_errors.get().saturating_add(1))")
        || !device.contains(
            "self.transmitted_errors.set(self.transmitted_errors.get().saturating_add(1))",
        )
        || !proc.contains("interface.received_errors,")
        || !proc.contains("interface.transmitted_errors,")
    {
        errors.push("network-failure: actual NIC diagnostics/counters cannot poison unrelated socket SO_ERROR; a failed device cannot erase successful completion notifications or RX backlog".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_device_owner_preserves_poll_outcome() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_erasing_other_device_backlog_and_counter_facts() {
        for (path, old, new) in [
            (
                "kernel/src/net/inet.rs",
                "poll.backlog || notification_backlog || cleanup_backlog",
                "notification_backlog || cleanup_backlog",
            ),
            (
                "kernel/src/fs/procfs/networking.rs",
                "interface.received_errors,",
                "0,",
            ),
            (
                "kernel/src/net/inet/poll.rs",
                "interface.device.take_error()",
                "None::<crate::devices::model::network::NetworkError>",
            ),
        ] {
            let root = super::super::repository_root();
            let mut sources = super::super::load_sources(&root).unwrap();
            let file = sources
                .iter_mut()
                .find(|file| file.relative == path)
                .unwrap();
            assert!(file.text.contains(old));
            file.text = file.text.replace(old, new);
            file.syntax = syn::parse_file(&file.text).unwrap();
            let mut errors = Vec::new();
            check(&sources, &mut errors);
            assert!(!errors.is_empty(), "{path}");
        }
    }
}
