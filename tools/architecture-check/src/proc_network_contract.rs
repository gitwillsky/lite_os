//! proc NET selector 与 per-open pin 必须使用同一 actual task/nsproxy/protocol owner。
use super::SourceFile;
fn source(sources: &[SourceFile], path: &str) -> String {
    sources
        .iter()
        .find(|source| source.relative == path)
        .expect("proc NET source")
        .text
        .split_whitespace()
        .collect()
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let proc = source(sources, "kernel/src/fs/procfs.rs");
    let node = source(sources, "kernel/src/fs/procfs/node.rs");
    let link = source(sources, "kernel/src/fs/procfs/link.rs");
    let opened = source(sources, "kernel/src/fs/procfs/opened_operations.rs");
    let provider = source(sources, "kernel/src/task/process_table/procfs.rs");
    let snapshot = source(sources, "kernel/src/fs/procfs/snapshot.rs");
    let config = source(sources, "kernel/src/socket/inet/configuration.rs");
    let task = source(sources, "kernel/src/task/model/namespaces.rs");
    let kernel = source(sources, "kernel/src/task/model/kernel_thread.rs");
    let table = source(sources, "kernel/src/fs/procfs/networking.rs");
    let generic = node
        .split("fngeneric_entries()")
        .nth(1)
        .unwrap_or("")
        .split("fngeneric_index(")
        .next()
        .unwrap_or("");
    if !link
        .contains("ProcNode::NetLink=>super::super::try_format_bytes(format_args!(\"self/net\"))")
        || node.contains("NetDir,")
        || generic.contains("Self::NetDev")
        || generic.contains("Self::NetRoute")
        || !proc.contains("ProcNode::TaskNetDir(tgid,tid,directory)")
        || !proc.contains("self.source.thread_net_namespace(tid).ok_or(FileSystemError::NotFound)?")
        || !proc.contains(
            "self.source.thread_net_namespace(tid).ok_or(FileSystemError::InvalidOperation)?",
        )
        || !proc.contains("Some(network.ok_or(FileSystemError::NotFound)?)")
        || !provider.contains("target.proc_net_namespace()")
    {
        errors.push("proc-net: root must resolve self/net and task directory must select actual live target NET without a caller or initial fallback".into());
    }
    if !opened.contains("source:Arc<dynsuper::ProcSource>")
        || !opened.contains("lettarget=inode.net_namespace")
        || !opened.contains(".and_then(Weak::upgrade).ok_or(FileSystemError::NoDevice)?")
        || !opened.contains("source.network_snapshot(target)?")
        || !opened.contains("source.route_snapshot(target)?")
        || snapshot.contains("ProcNetworkSnapshot")
        || provider.contains("letnet=current")
        || config.contains(".find(|interface|interface.index!=1)")
        || !config.contains("network.interfaces.iter().map(|interface|NetworkSnapshot")
        || !table.contains("forinterfaceininterfaces{")
        || !table.contains("output.write_bytes(name)?")
        || !table.contains("interface.transmitted_dropped")
    {
        errors.push("proc-net: opened table must pin exact child NET and read all actual links/drop counters separately from global Process/MM snapshots".into());
    }
    if !kernel.contains("namespaces:Mutex<Option<super::namespaces::NamespaceMembership>>")
        || !kernel.contains("namespaces:Mutex::new(Some(namespaces))")
        || !kernel.contains("self.namespaces.lock().take()")
        || !task
            .contains("TaskKind::Kernel(thread)=>thread.namespaces.lock().as_ref().map(project)")
        || !task.contains("self.with_proc_namespaces(|membership|membership.net.clone())")
        || !task.contains(
            "scalar.or_else(||selector.and_then(|selector|membership.reference(selector)))",
        )
        || task.contains("returnSome(NamespaceReference::new(crate::namespace::initial_")
    {
        errors.push("proc-net: kernel workers must own and retire actual nsproxy references; proc/PIDFD may not fabricate initial namespace after exit".into());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Vec<SourceFile> {
        super::super::load_sources(&super::super::repository_root()).unwrap()
    }
    #[test]
    fn actual_proc_net_owner_and_lifetime_satisfy_contract() {
        let mut errors = Vec::new();
        check(&fixture(), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_caller_selection_first_link_and_unretired_kernel_proxy() {
        for (path, old, new) in [
            (
                "kernel/src/task/process_table/procfs.rs",
                "target.proc_net_namespace()",
                "Some(crate::socket::initial_net())",
            ),
            (
                "kernel/src/fs/procfs/opened_operations.rs",
                "source.network_snapshot(target)?",
                "source.network_snapshot(&crate::socket::initial_net())?",
            ),
            (
                "kernel/src/socket/inet/configuration.rs",
                "network.interfaces.iter().map(|interface| NetworkSnapshot",
                "network.interfaces.iter().take(1).map(|interface| NetworkSnapshot",
            ),
            (
                "kernel/src/task/model/kernel_thread.rs",
                ".take()\n            .expect(\"kernel nsproxy retired twice\")",
                ".clone()\n            .expect(\"kernel nsproxy retired twice\")",
            ),
        ] {
            let mut sources = fixture();
            let source = sources
                .iter_mut()
                .find(|source| source.relative == path)
                .unwrap();
            assert!(source.text.contains(old), "{path}: {old}");
            source.text = source.text.replace(old, new);
            source.syntax = syn::parse_file(&source.text).unwrap();
            let mut errors = Vec::new();
            check(&sources, &mut errors);
            assert!(
                errors.iter().any(|error| error.starts_with("proc-net:")),
                "{errors:?}"
            );
        }
    }
}
