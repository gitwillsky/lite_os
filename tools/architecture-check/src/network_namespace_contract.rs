//! 实际网络域准备收费与UNIX范围：abstract用被创建socket的域，pathname按真实inode。
use super::SourceFile;
fn normalized(source: &str) -> String {
    source.split_whitespace().collect()
}
fn source<'a>(sources: &'a [SourceFile], path: &str) -> &'a str {
    &sources
        .iter()
        .find(|file| file.relative == path)
        .expect("network namespace source")
        .text
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let owner = normalized(source(sources, "kernel/src/net/net_namespace.rs"));
    let protocol_owner = normalized(source(sources, "kernel/src/net/inet/protocol_owner.rs"));
    let inet = normalized(source(sources, "kernel/src/net/inet.rs"));
    let configuration = normalized(source(sources, "kernel/src/net/inet/configuration.rs"));
    let timing = normalized(source(sources, "kernel/src/net/inet/timing.rs"));
    let interface = normalized(source(sources, "kernel/src/syscall/socket/interface.rs"));
    let registry = normalized(source(sources, "kernel/src/net/unix/namespace.rs"));
    let unix = normalized(source(sources, "kernel/src/net/unix.rs"));
    let lifecycle = normalized(source(sources, "kernel/src/net/unix/lifecycle.rs"));
    let proc = normalized(source(sources, "kernel/src/fs/procfs.rs"));
    let opened = normalized(source(sources, "kernel/src/fs/procfs/opened_operations.rs"));
    let nodes = normalized(source(sources, "kernel/src/fs/procfs/node.rs"));
    let generic = nodes
        .split("fngeneric_entries()")
        .nth(1)
        .unwrap_or("")
        .split("pub(super)fngeneric_index(")
        .next()
        .unwrap_or("");
    if !owner.contains(
        "UserResourceCharge::acquire(&user,creator_uid,crate::namespace::UserResource::Net",
    ) || !owner.contains("_charge:Some(charge)")
        || !owner.contains("id:crate::namespace::next_namespace_id()")
        || !owner.contains("protocol:Once<Arc<super::inet::NetworkStackOwner>>")
        || !owner.contains("INITIAL_NET.try_call_once(||{letinitial=Arc::try_new(NetNamespace{")
        || !owner.contains("publish(initial)}).map(|_|())")
        || owner.contains("INITIAL_NET.call_once(")
        || !owner.contains("self.protocol.call_once(||owner)")
        || protocol_owner.contains("staticNETWORK_STACK")
        || !inet.contains("scope.install_protocol(owner)")
        || !inet.contains("stack(self.memory.namespace_ref())")
        || !inet.contains("self.memory.namespace_ref().protocol()?.try_observe()")
        || !inet.contains("ifletSome(stack)=self.memory.namespace_ref().protocol()")
        || !owner.contains("entries:crate::fallible_tree::FallibleMap<u64,Weak<NetNamespace>>")
        || !owner.contains("letmutdirectory=NETWORK_NAMESPACES.try_lock()?")
        || !owner.contains("NETWORK_NAMESPACE_BUDGET:usize=8;")
        || !owner.contains("directory.entries.successor(&cursor)")
        || !owner.contains("NetworkNamespaceScan::Deferred=>directory.deferred_cursor=cursor")
        || !owner.contains("NetworkNamespaceScan::Timer=>directory.timer_cursor=cursor")
        || !owner.contains("NETWORK_NAMESPACES.lock().entries.take_entry(&self.id)")
        || !inet.contains("take_network_namespaces(NetworkNamespaceScan::Deferred)")
        || !inet.contains("letSome(stack)=namespace.protocol()")
        || !timing.contains("take_network_namespaces(NetworkNamespaceScan::Timer)")
        || !timing.contains("letSome(stack)=namespace.protocol()")
        || timing.contains("initial_stack(")
        || configuration.contains("initial_net_ref(")
        || !configuration.contains("letnetwork=stack(scope)?.lock()?")
        || !interface.contains("letnet=socket.net_namespace();")
        || !interface.contains("&scope.user_namespace(),syscall_abi::capability::Capability::NET_ADMIN")
        || !owner.contains("authority.effective_uid==0")
        || owner.contains("self.user.to_kernel(true,0)")
        || !registry.contains("Abstract(u64,UnixAddress,SocketType)")
        || !registry.contains("NamespaceKey::Abstract(net_id,address,socket_type)")
        || !registry.contains("request.candidate(socket.network_namespace_id(),socket.socket_type,")
        || !unix.contains("net_namespace:Arc<super::NetNamespace>")
        || !unix.contains(
            "NamespaceKey::Abstract(self.network_namespace_id(),address,self.socket_type)",
        )
        || !unix.contains("NamespaceKey::Abstract(net_namespace.id(),*address,socket_type")
        || !unix.contains("namespace::lookup(&NamespaceKey::Path(identity))")
        || !lifecycle.contains("listener.net_namespace.clone(),")
        || !proc.contains(
            "letscope=net_namespace.as_ref().map_or(scope,|namespace|namespace.id())",
        )
        || !opened.contains("target.is_initial()&&target.may_set_limits(authority)")
        || !proc.contains("ifmatches!(node,ProcNode::CoreBufferLimit(_))&&!namespace.is_initial(){initial.mode&=!0o222;")
        || generic.contains("Self::CoreBufferLimit(")
        || generic.contains("Self::OptmemLimit")
        || generic.contains("Self::NetSysctlDir")
        || generic.contains("Self::CoreNetSysctlDir")
    {
        errors.push("network-namespace: one actual NET lifetime must charge ucounts, partition abstract keys using immutable socket scope, preserve global inode lookup and capture scoped proc policy with readonly global limits".into());
    }
    for path in [
        "kernel/src/net/inet/options.rs",
        "kernel/src/net/inet/udp.rs",
        "kernel/src/net/inet/raw.rs",
        "kernel/src/net/inet/tcp.rs",
        "kernel/src/net/inet/tcp/io.rs",
        "kernel/src/net/inet/tcp/accept.rs",
        "kernel/src/net/inet/tcp/lifecycle.rs",
        "kernel/src/net/inet/wait.rs",
    ] {
        if normalized(source(sources, path)).contains("initial_stack(") {
            errors.push(format!("network-namespace: {path}: existing endpoint operation/loan/readiness must use captured NET, never initial/current Task fallback"));
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_network_policy_and_unix_namespace_preparation_satisfy_contract() {
        let sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn unscoped_abstract_missing_quota_and_writable_global_tunables_are_rejected() {
        for (path, old, new) in [
            (
                "kernel/src/net/net_namespace.rs",
                ".try_call_once(|| {",
                ".call_once(|| {",
            ),
            (
                "kernel/src/net/net_namespace.rs",
                "publish(initial)",
                "Ok(initial)",
            ),
            (
                "kernel/src/net/unix/namespace.rs",
                "NamespaceKey::Abstract(net_id, address, socket_type)",
                "NamespaceKey::Abstract(6, address, socket_type)",
            ),
            (
                "kernel/src/net/net_namespace.rs",
                "_charge: Some(charge)",
                "_charge: None",
            ),
            (
                "kernel/src/fs/procfs/opened_operations.rs",
                "target.is_initial() && target.may_set_limits(authority)",
                "target.may_set_limits(authority)",
            ),
            (
                "kernel/src/net/net_namespace.rs",
                "FallibleMap<u64, Weak<NetNamespace>>",
                "FallibleMap<u64, Arc<NetNamespace>>",
            ),
            (
                "kernel/src/net/net_namespace.rs",
                "directory.deferred_cursor = cursor",
                "directory.deferred_cursor = 0",
            ),
            (
                "kernel/src/syscall/socket/interface.rs",
                "&scope.user_namespace()",
                "&crate::namespace::initial_user()",
            ),
        ] {
            let mut sources = super::super::load_sources(&super::super::repository_root()).unwrap();
            let actual = sources
                .iter_mut()
                .find(|file| file.relative == path)
                .unwrap();
            assert!(actual.text.contains(old));
            actual.text = actual.text.replace(old, new);
            syn::parse_file(&actual.text).unwrap();
            let mut errors = Vec::new();
            check(&sources, &mut errors);
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains("network-namespace:")),
                "{errors:?}"
            );
        }
    }
}
