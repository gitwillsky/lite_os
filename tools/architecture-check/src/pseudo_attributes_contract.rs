//! Pseudo 属性必须落在实际 inode/registration owner，不能用 Snapshot kind 或零截断旁路。
use super::SourceFile;
fn source<'a>(sources: &'a [SourceFile], path: &str) -> &'a str {
    &sources
        .iter()
        .find(|source| source.relative == path)
        .expect("pseudo attributes source")
        .text
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let cache = source(sources, "kernel/src/fs/page_cache.rs");
    let identity = source(sources, "kernel/src/fs/inode_identity.rs");
    let proc = source(sources, "kernel/src/fs/procfs.rs");
    let attributes = source(sources, "kernel/src/fs/procfs/attributes.rs");
    let nsfs = source(sources, "kernel/src/fs/nsfs.rs");
    let opened = source(sources, "kernel/src/fs/procfs/opened_operations.rs");
    let node = source(sources, "kernel/src/fs/procfs/node.rs");
    let credentials = source(sources, "kernel/src/execution/task/model/credentials.rs");
    if !cache
        .contains("DataBacking::Snapshot => return inode.truncate_storage(size, Some(identity))")
    {
        errors.push("pseudo-attributes: Snapshot truncate must dispatch actual inode policy for every legal size".into());
    }
    if !identity.contains("table.take_entry(&number)")
        || !identity.contains("core::ptr::eq(live.as_ptr(), owner)")
        || !proc.contains("self.identities.live.retire(self.key, self)")
        || !nsfs.contains("INODES.retire(self.namespace.inode(), self)")
        || !nsfs.contains("INODES.admit(number, &candidate)")
        || !opened.contains("inode: inode.shared()")
    {
        errors.push("pseudo-attributes: aliases must share live inode with exact lock-external identity retirement".into());
    }
    if !attributes
        .contains("static REGISTERED_ENTRIES: Once<[Owner; ProcNode::GENERIC_ENTRY_COUNT]>")
        || !attributes.contains("node.generic_index()")
        || !attributes.contains("node.owner_tid().is_some() && change.changes_mode()")
        || !proc.contains("owner.unwrap_or((0, 0))")
        || !proc.contains("source.thread_inode_owner(tid, node.metadata_mode())")
        || !credentials.contains("mode == 0o040555")
        || !credentials.contains("namespace.to_kernel(true, 0).unwrap_or(0)")
        || !node.contains("TaskFd(usize, usize)")
        || !proc.contains("let key = (node, scope)")
        || !proc.contains("identities.live.admit(key, &candidate)")
        || node.contains("(pid as u64) <<")
    {
        errors.push("pseudo-attributes: registration must retain generic mode/owner; PID projection preserve retired getattr, dumpability and full FD identity".into());
    }
}
#[cfg(test)]
mod tests {
    fn reject(path: &str, old: &str, new: &str, expected: &str) {
        let mut sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == path)
            .unwrap();
        assert!(source.text.contains(old));
        source.text = source.text.replace(old, new);
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(
            errors.iter().any(|error| error.contains(expected)),
            "{errors:?}"
        );
    }
    #[test]
    fn rejects_blanket_snapshot_read_only() {
        reject(
            "kernel/src/fs/page_cache.rs",
            "DataBacking::Snapshot => return inode.truncate_storage(size, Some(identity))",
            "DataBacking::Snapshot => return Err(FileSystemError::ReadOnly)",
            "every legal size",
        );
    }
    #[test]
    fn rejects_late_old_inode_drop_deleting_new_winner() {
        reject(
            "kernel/src/fs/inode_identity.rs",
            "core::ptr::eq(live.as_ptr(), owner)",
            "true",
            "identity retirement",
        );
    }
    #[test]
    fn rejects_truncated_fd_identity() {
        reject(
            "kernel/src/fs/procfs/node.rs",
            "TaskFd(usize, usize)",
            "TaskFd(usize, u16)",
            "full FD identity",
        );
    }
    #[test]
    fn actual_pseudo_attributes_have_one_identity_owner() {
        let sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
}
