//! NEWNET必须先完成actual protocol准备，再无失败移交唯一Task nsproxy；nsfs/PIDFD不得重建域。
use super::SourceFile;
fn source(sources: &[SourceFile], path: &str) -> String {
    sources
        .iter()
        .find(|source| source.relative == path)
        .expect("NEWNET publication source")
        .text
        .split_whitespace()
        .collect()
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let task = source(sources, "kernel/src/execution/task/model/namespaces.rs");
    let inet = source(sources, "kernel/src/net/inet.rs");
    let reference = source(sources, "kernel/src/namespace/reference.rs");
    let owner = source(sources, "kernel/src/net/net_namespace.rs");
    let clone = source(sources, "kernel/src/syscall/process.rs");
    let unshare = source(sources, "kernel/src/syscall/process_context.rs");
    let pidfd = source(sources, "kernel/src/fs/pidfs/ioctl.rs");
    let factory = inet
        .split("fncreate_net_namespace(")
        .nth(1)
        .unwrap_or("")
        .split("pub(crate)fninit(")
        .next()
        .unwrap_or("");
    if !factory.contains("super::NetNamespace::create(user,creator_uid)?")
        || !factory.contains("prepare_network(&namespace,&[],false)")
        || !factory.contains("namespace.install_protocol(owner);Ok(namespace)")
        || !task.contains("membership.net=crate::net::create_net_namespace(owner,authority.effective_uid)?")
        || !clone.contains("|NEWNET|") || !clone.contains("flags&NEWNET!=0,")
        || !unshare.contains("flags&CLONE_NEWNET!=0,")
        || !task.contains("NamespaceType::Net=>")
        || !task.contains("target.downcast::<crate::net::NetNamespace>()?")
        || !task.contains("!authority.capable(&net.user_namespace(),syscall_abi::capability::Capability::SYS_ADMIN,)")
        || !task.contains(".expect(\"livenamespacemembership\").net,net,")
    {
        errors.push("newnet: native clone/unshare must prepare the real empty DOWN-lo protocol before membership commit; setns consumes exact typed owner with both SYS_ADMIN checks".into());
    }
    if !owner.contains("implcrate::namespace::NamespaceOwnerforNetNamespace")
        || !reference.contains("Self::Net=>0x4000_0000")
        || !reference.contains("Self::Net=>b\"net\"")
        || !reference.contains("(NamespaceType::Net,6)=>0xefff_fffb")
        || !task.contains("NamespaceSelector::Namespace(NamespaceType::Net)=>")
        || !task.contains("NamespaceReference::new(self.net.clone())")
        || !pidfd.contains("0xff04=>Some(Selector::Namespace(Kind::Net))")
    {
        errors.push("newnet: proc/nsfs/PIDFD must retain the same concrete NET identity and owning UserNS using only native flags/inodes/selectors".into());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_newnet_constructor_and_publication_satisfy_contract() {
        let sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_protocol_free_namespace_and_initial_owner_selection() {
        for (path, old, new) in [
            (
                "kernel/src/net/inet.rs",
                "prepare_network(&namespace, &[], false)",
                "prepare_network(&namespace, &[], true)",
            ),
            (
                "kernel/src/execution/task/model/namespaces.rs",
                "NamespaceReference::new(self.net.clone())",
                "NamespaceReference::new(crate::net::initial_net())",
            ),
            (
                "kernel/src/fs/pidfs/ioctl.rs",
                "0xff04 => Some(Selector::Namespace(Kind::Net))",
                "0xff04 => None",
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
                errors.iter().any(|error| error.starts_with("newnet:")),
                "{errors:?}"
            );
        }
    }
}
