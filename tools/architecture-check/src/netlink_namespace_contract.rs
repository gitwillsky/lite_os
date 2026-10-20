//! NETLINK_KOBJECT_UEVENT：actual NET/port owner、native bind、FIFO预算与锁外通知。
use super::SourceFile;
fn source(sources: &[SourceFile], path: &str) -> String {
    sources
        .iter()
        .find(|source| source.relative == path)
        .expect("netlink actual source")
        .text
        .split_whitespace()
        .collect()
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let endpoint = source(sources, "kernel/src/socket/kobject.rs");
    let preparation = source(sources, "kernel/src/socket/preparation.rs");
    let bind = source(sources, "kernel/src/syscall/socket.rs");
    let observation = source(sources, "kernel/src/socket/observation.rs");
    if !endpoint.contains("memory:Arc<SocketMemory>")
        || !endpoint.contains("ports:FallibleMap<(u8,u64,u32),u64>")
        || !endpoint.contains("letnet=self.memory.namespace_ref().id();")
        || !endpoint.contains("ports.contains_key(&(protocol,net,port))")
        || !endpoint.contains("choose_port(&registry.ports,PROTOCOL_KOBJECT_UEVENT,net,self.id,address.port_id,caller_tgid,)")
        || !endpoint.contains("groups:address.groups&KOBJECT_UEVENT_GROUP")
        || !endpoint.contains("ifaddress.port_id!=bound.port_id{")
        || !endpoint.contains("caller_tgid")
        || !preparation.contains("KobjectSocket::new(notify,self.memory.clone())")
        || !bind.contains("letcaller_tgid=task.visible_tgid();")
        || bind.contains("SocketAddress::Netlink(NetlinkAddress{port_id,groups})")
    {
        errors.push("netlink-domain: same actual SocketMemory NET must own scoped port index/native group rebind; syscall supplies visible TGID without rewriting port0".into());
    }
    if endpoint.contains("QUEUE_CAPACITY")
        || endpoint.contains(".retain(")
        || !endpoint.contains("queue:FallibleMap<u64,Uevent>")
        || !endpoint.contains("state.queue.take_entry(&key)")
        || !endpoint.contains("state.receive_used-=VacantEntry::<u64,Uevent>::allocation_size();")
        || !endpoint.contains("ifnotify{state.pending_error=true;}")
        || !endpoint
            .contains("core::mem::take(&mutstate.pending_error).then_some(SocketError::NoBuffer)")
        || !endpoint.contains("self.state.try_lock()?")
        || !observation.contains("SocketBackend::Kobject(socket)=>socket.try_poll_state()")
    {
        errors.push("netlink-domain: actual allocated FIFO budget must reject new events without overwriting queued payload, report native once-per-overrun ENOBUFS and support no-wait observation".into());
    }
    let broadcast = endpoint
        .split("fnpublish_drm_hotplug()")
        .nth(1)
        .unwrap_or("");
    if !broadcast.contains("endpoint.memory.namespace_ref().user_namespace().is_initial()")
        || broadcast.contains("namespace_ref().is_initial()")
        || !broadcast.contains("letnext={letregistry=registry().lock();")
        || !broadcast.contains(".map(|(&id,endpoint)|(id,endpoint.upgrade()))};")
        || !broadcast.contains("endpoint.enqueue(event);")
        || !broadcast.contains("drop(endpoint);")
        || !endpoint.contains("drop((endpoint,port,state));")
    {
        errors.push("netlink-domain: untagged kernel event must reach every initial-UserNS-owned NET, with delivery/notifications/final endpoint and namespace references outside registry guard".into());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_netlink_scope_bind_queue_and_guard_contract() {
        let sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_global_port_initial_net_only_delivery_and_blocking_observation() {
        for (path, old, new) in [
            (
                "kernel/src/socket/kobject.rs",
                "let net = self.memory.namespace_ref().id();",
                "let net = 6;",
            ),
            (
                "kernel/src/socket/kobject.rs",
                ".user_namespace()\n                .is_initial()",
                ".is_initial()",
            ),
            (
                "kernel/src/socket/observation.rs",
                "SocketBackend::Kobject(socket) => socket.try_poll_state(),",
                "SocketBackend::Kobject(socket) => Some(socket.poll_state()),",
            ),
        ] {
            let mut sources = super::super::load_sources(&super::super::repository_root()).unwrap();
            let source = sources
                .iter_mut()
                .find(|source| source.relative == path)
                .unwrap();
            assert!(source.text.contains(old), "{old}");
            source.text = source.text.replace(old, new);
            source.syntax = syn::parse_file(&source.text).unwrap();
            let mut errors = Vec::new();
            check(&sources, &mut errors);
            assert!(
                errors
                    .iter()
                    .any(|error| error.starts_with("netlink-domain:")),
                "{errors:?}"
            );
        }
    }
}
