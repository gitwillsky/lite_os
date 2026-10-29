//! actual设备MTU归属和native ioctl→packet/protocol机制；不允许恒定getter/空setter。
use super::SourceFile;
fn source(sources: &[SourceFile], path: &str) -> String {
    sources
        .iter()
        .find(|source| source.relative == path)
        .expect("actual MTU source")
        .text
        .split_whitespace()
        .collect()
}
pub(super) fn check(root: &std::path::Path, sources: &[SourceFile], errors: &mut Vec<String>) {
    let config = source(sources, "kernel/src/net/inet/configuration.rs");
    let device = source(sources, "kernel/src/net/inet/device.rs");
    let loopback = source(sources, "kernel/src/net/inet/device/loopback.rs");
    let ioctl = source(sources, "kernel/src/syscall/socket/interface.rs");
    let packet = source(sources, "kernel/src/net/packet.rs");
    let mechanism = std::fs::read_to_string(root.join("vendor/smoltcp/src/iface/interface/mod.rs"))
        .expect("pinned MTU mechanism")
        .split_whitespace()
        .collect::<String>();
    if !config.contains("mtu:interface.device.ip_mtu()asi32")
        || !config.contains("interface.device.configure_mtu(mtu)?;")
        || !config.contains(
            "interface.interface.set_device_capabilities(interface.device.capabilities());",
        )
        || !device.contains("capabilities.max_transmission_unit=self.mtu+14;")
        || !loopback.contains("capabilities.max_transmission_unit=self.mtu;")
        || !mechanism.contains("self.inner.caps=caps;")
        || !device.contains("length<=self.maximum_frame_length")
        || !loopback.contains("ifpacket.len()>self.mtu{")
        || !packet.contains("input.len()>interface.mtuasusize+")
        || !ioctl.contains("SIOCSIFMTU=>configure_mtu(scope,index,")
    {
        errors.push("interface-mtu: getter, native setter, actual device token/packet limit and cached protocol segmentation caps must use one configured MTU owner".into());
    }
    if !device.contains("Self::Ethernet(_)=>(68,ETHERNET_MTUasi32-14)")
        || !device.contains("Self::Loopback(_)=>(0,loopback::LOOPBACK_MTUasi32)")
        || !config.contains("ifmtu<68{")
        || !config.contains("interface.state.address=None;")
        || !config.contains("interface.device.ip_mtu()>=68")
        || !config.contains("ifinterface.device.ip_mtu()<68{returnErr(SocketError::NoBuffer);}")
    {
        errors.push("interface-mtu: native device ranges and lo IPv4 teardown/re-enable must retire actual address, forbid too-small IPv4 and never recreate old address merely on MTU growth".into());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_mtu_owner_and_native_teardown_satisfy_contract() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).unwrap();
        let mut errors = Vec::new();
        check(&root, &sources, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_constant_projection_and_missing_protocol_refresh() {
        for (old, new) in [
            ("mtu: interface.device.ip_mtu() as i32,", "mtu: 1500,"),
            (
                ".set_device_capabilities(interface.device.capabilities());",
                ".set_interface_index(interface.index);",
            ),
        ] {
            let root = super::super::repository_root();
            let mut sources = super::super::load_sources(&root).unwrap();
            let source = sources
                .iter_mut()
                .find(|source| source.relative == "kernel/src/net/inet/configuration.rs")
                .unwrap();
            assert!(source.text.contains(old));
            source.text = source.text.replace(old, new);
            source.syntax = syn::parse_file(&source.text).unwrap();
            let mut errors = Vec::new();
            check(&root, &sources, &mut errors);
            assert!(
                errors
                    .iter()
                    .any(|error| error.starts_with("interface-mtu:")),
                "{errors:?}"
            );
        }
    }
}
