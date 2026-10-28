//! Caller fs root capability、唯一 traversal 与 proc viewer path 的 owner 围栏。
use super::SourceFile;
use quote::ToTokens;
use syn::Item;
const LOOKUP: &str = "kernel/src/fs/vfs/path_lookup.rs";
const CONTEXT: &str = "kernel/src/task/model/process_resources.rs";
const OPENED: &str = "kernel/src/fs/vfs/opened.rs";
fn source<'a>(sources: &'a [SourceFile], path: &str) -> &'a SourceFile {
    sources
        .iter()
        .find(|source| source.relative == path)
        .expect("fs root source")
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let vfs = source(sources, "kernel/src/fs/vfs.rs");
    let namespace = source(sources, "kernel/src/fs/vfs/mount_namespace.rs");
    let vfs_text: String = vfs.text.split_whitespace().collect();
    let namespace_text: String = namespace.text.split_whitespace().collect();
    if ![
        "initial:Arc<MountNamespace>",
        "initial:MountNamespace::initial(scope)?",
        "self.initial.clone()",
        "self.initial.install_root(view)",
    ]
    .iter()
    .all(|fact| vfs_text.contains(fact))
        || ![
            "root:Once<Arc<MountView>>",
            "root:Once::new()",
            "self.root.try_call_once(||",
            "self.root.get().ok_or(FileSystemError::NotFound)?",
            "self.root.get().map(Arc::clone)",
        ]
        .iter()
        .all(|fact| namespace_text.contains(fact))
    {
        errors.push("fs-root: initial mount namespace must exist before worker creation and keep the same identity through one-time root publication".into());
    }
    let boot: String = source(sources, "kernel/src/main.rs")
        .text
        .split_whitespace()
        .collect();
    if !boot.contains("fs::MountFlags::from_bits(fs::MountFlags::default().bits()|ifparameters.read_only_root{u64::from(fs::MountFlags::READ_ONLY)}else{0},)") {
        errors.push("fs-root: readonly boot must add READ_ONLY to the default root mount attributes, preserving relatime through remount".into());
    }
    let context = source(sources, CONTEXT);
    let shape = context.syntax.to_token_stream().to_string();
    for fact in [
        "root : Arc < OpenedFile >",
        "context . root . clone ()",
        "context . cwd . clone ()",
        "drop (retired)",
    ] {
        if !shape.contains(fact) {
            errors.push(format!(
                "fs-root: one context and coherent root/cwd snapshot require {fact}"
            ));
        }
    }
    for path in [
        LOOKUP,
        "kernel/src/fs/vfs/mutation.rs",
        "kernel/src/fs/vfs.rs",
    ] {
        let source = source(sources, path);
        for item in &source.syntax.items {
            let Item::Impl(item) = item else {
                continue;
            };
            for item in &item.items {
                let syn::ImplItem::Fn(function) = item else {
                    continue;
                };
                let name = function.sig.ident.to_string();
                if !name.ends_with("_at")
                    && !name.ends_with("_at_no_follow")
                    && name != "resolve_from"
                    && name != "resolve_from_with_limit"
                    && name != "parent_from"
                {
                    continue;
                }
                for argument in &function.sig.inputs {
                    let syn::FnArg::Typed(argument) = argument else {
                        continue;
                    };
                    let syn::Pat::Ident(pattern) = argument.pat.as_ref() else {
                        continue;
                    };
                    if (pattern.ident == "origin" || pattern.ident.to_string().ends_with("start"))
                        && argument.ty.to_token_stream().to_string() != "LookupOrigin"
                    {
                        errors.push(format!("fs-root: {path}::{name} may not fall back from optional start to global root"));
                    }
                }
            }
        }
    }
    let lookup = source(sources, LOOKUP);
    if !matches!((lookup.text.find("if opened.same_path(&root)"),lookup.text.find("opened.parent(&root)?")),(Some(root),Some(parent)) if root<parent)
        || lookup.text.matches("self.root_opened()?").count() != 1
        || !lookup.text.contains("pub(crate) fn initial_origin(&self)")
        || lookup.text.matches("LookupOrigin::new(root,").count() != 2
        || !lookup
            .text
            .contains("if let Some(parent) = opened.parent(&root)?")
        || !lookup
            .text
            .contains("opened.inode().inode_type() != InodeType::Directory")
    {
        errors.push("fs-root: canonical traversal must clamp before mount exit and preserve root through symlink recursion".into());
    }
    for source in sources
        .iter()
        .filter(|source| source.relative.starts_with("kernel/src/syscall/"))
    {
        if source.text.contains("task.working_directory()") || source.text.contains(".open_file(") {
            errors.push(format!(
                "fs-root: {} user pathname must carry caller origin",
                source.relative
            ));
        }
    }
    if !lookup.text.contains("remaining.push(b'/')")
        || source(sources, OPENED)
            .text
            .matches("&b\"(unreachable)\"[..]")
            .count()
            != 1
    {
        errors.push("fs-root: magic suffix preserves trailing slash and outside cwd emits one preallocated prefix".into());
    }
    let namespace = source(sources, "kernel/src/fs/vfs/namespace_access.rs");
    if !namespace.text.contains("_guard: TaskMutexGuard<'a, ()>")
        || !lookup.text.contains("impl NamespaceAccess<'_>")
        || !lookup.text.contains("self.namespace()?")
        || !source(sources, "kernel/src/fs/vfs/mutation.rs")
            .text
            .contains("namespace.create_entry(")
        || !source(sources, "kernel/src/fs/procfs.rs")
            .text
            .contains("size: node.metadata_size(),")
    {
        errors.push("fs-root: admission/projection need one namespace capability; proc metadata must not reacquire it".into());
    }
    let inode = source(sources, "kernel/src/fs/inode.rs");
    let proc_inode = source(sources, "kernel/src/fs/procfs/link.rs");
    let fd_producer = source(sources, "kernel/src/task/model/file_table.rs");
    if !inode.text.contains("pub(crate) enum LinkTarget")
        || !lookup.text.contains("match opened.inode().link_target()?")
        || lookup.text.contains("opened.inode().read_link()")
        || proc_inode.text.contains("fn follow_link")
        || !proc_inode
            .syntax
            .to_token_stream()
            .to_string()
            .contains(". thread_file_descriptors (pid) ?")
        || !fd_producer
            .text
            .contains("Result<alloc::vec::Vec<crate::fs::ProcFileDescriptorSnapshot>, crate::fs::FileSystemError>")
        || fd_producer.text.contains("ofd.proc_target().ok()")
    {
        errors.push("fs-root: link target must freeze kind/identity once and propagate producer failures without retry".into());
    }
    let loader = source(sources, "kernel/src/task/loader.rs");
    if loader.text.matches(".open_file_at(origin.clone(),").count() != 2 {
        errors.push("fs-root: main/script and PT_INTERP must use the same caller origin".into());
    }
    let opened = source(sources, OPENED);
    if !opened
        .text
        .contains("Arc::ptr_eq(&self.dentry, &other.dentry)")
        || !opened
            .text
            .contains("(Some(left), Some(right)) => Arc::ptr_eq(left, right)")
        || !opened
            .text
            .contains("same_mount && Arc::ptr_eq(&dentry, &root.dentry)")
        || !opened.text.contains("!reached_root && !deleted_suffix")
        || !opened.text.contains("b\"(unreachable)\"")
        || opened.text.contains("inode_identity().ok()")
    {
        errors.push("fs-root: opened path identity requires actual dentry and mount; stop at caller root and distinguish unreachable cwd from proc target".into());
    }
    let parent = opened
        .text
        .split_once("pub(super) fn parent(")
        .map(|(_, body)| {
            body.split_once("pub(crate) fn watch_context")
                .map_or(body, |(body, _)| body)
        });
    if !parent.is_some_and(|body| {
        matches!((body.find("Arc::ptr_eq(&stop.dentry, &dentry)"),body.find("view.attachment()")),(Some(root),Some(mount)) if root<mount)
            && body.contains("Arc::ptr_eq(root, &view)")
            && body.contains("return Ok(Some(stop.clone()))")
    }) {
        errors.push("fs-root: stacked mount parent traversal must clamp actual caller path before each attachment escape".into());
    }
    let syscall = source(sources, "kernel/src/syscall/fs/open.rs");
    if !matches!((syscall.text.find("pub(crate) fn sys_chroot"),syscall.text.rfind("inode.require_access(&identity, 1 | crate::fs::MAY_CHDIR)"),syscall.text.find("Capability::SYS_CHROOT"),syscall.text.find("task.set_root_directory(opened)")),(Some(start),Some(search),Some(cap),Some(commit)) if start<search && search<cap && cap<commit)
    {
        errors.push(
            "fs-root: chroot must authorize pathname/search before capability and commit only root"
                .into(),
        );
    }
    let proc = source(sources, "kernel/src/task/process_table/procfs.rs");
    if !proc
        .text
        .split_whitespace()
        .collect::<String>()
        .contains("super::thread_selector::execution_by_tid(&graph,tid)")
        || !proc.text.contains("caller.may_inspect_thread(&target)")
        || !source(sources, "kernel/src/fs/procfs/snapshot.rs")
            .text
            .contains("pub(crate) target: Arc<super::super::OpenedFile>")
        || !source(sources, "kernel/src/fs/procfs/link.rs")
            .syntax
            .to_token_stream()
            .to_string()
            .contains(". current_root ()")
        || !fd_producer.text.contains("target: ofd.opened_ref(),")
    {
        errors.push("fs-root: proc directory requires live main Thread; all targets use viewer root from upper producer".into());
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
            errors.iter().any(|error| error.contains(expected)),
            "{errors:?}"
        );
    }
    #[test]
    fn accepts_explicit_root_owners() {
        let mut errors = Vec::new();
        check(&fixture(), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_optional_legacy_start() {
        reject(
            LOOKUP,
            "origin: LookupOrigin",
            "origin: Option<Arc<OpenedFile>>",
            "fall back",
        );
    }
    #[test]
    fn rejects_root_loss_at_absolute_symlink() {
        reject(
            LOOKUP,
            "LookupOrigin::new(root, parent)",
            "LookupOrigin::new(global_root, parent)",
            "symlink recursion",
        );
    }
    #[test]
    fn rejects_root_comparison_after_mount_escape() {
        reject(
            LOOKUP,
            "if opened.same_path(&root)",
            "if opened.same_path(&global_root)",
            "clamp before mount",
        );
    }
    #[test]
    fn rejects_interpreter_using_boot_root() {
        reject(
            "kernel/src/task/loader.rs",
            ".open_file_at(origin.clone(), &path, identity)",
            ".open_file_at(global_origin, &path, identity)",
            "PT_INTERP",
        );
    }
    #[test]
    fn rejects_proc_worker_replacing_exited_leader_fs() {
        reject(
            "kernel/src/task/process_table/procfs.rs",
            "super::thread_selector::execution_by_tid(&graph, tid)",
            "arbitrary_process_thread(&graph, tid)",
            "live main Thread",
        );
    }
    #[test]
    fn rejects_equal_dentry_across_distinct_mount_views() {
        reject(
            OPENED,
            "(Some(left), Some(right)) => Arc::ptr_eq(left, right)",
            "(Some(_), Some(_)) => true",
            "opened path",
        );
    }
    #[test]
    fn rejects_stacked_parent_escape_past_caller_mount() {
        reject(
            OPENED,
            "Arc::ptr_eq(&stop.dentry, &dentry)",
            "Arc::ptr_eq(&global_root.dentry, &dentry)",
            "stacked mount parent",
        );
    }
    #[test]
    fn rejects_readonly_boot_discarding_default_mount_attributes() {
        reject(
            "kernel/src/main.rs",
            "fs::MountFlags::default().bits()",
            "0u64",
            "preserving relatime",
        );
    }
    #[test]
    fn rejects_initial_mount_membership_waiting_for_root_mount() {
        reject(
            "kernel/src/fs/vfs.rs",
            "initial: Arc<MountNamespace>",
            "initial: Once<Arc<MountNamespace>>",
            "before worker creation",
        );
        reject(
            "kernel/src/fs/vfs.rs",
            "self.initial.install_root(view)",
            "let _ = view; Ok(())",
            "before worker creation",
        );
    }
    #[test]
    fn rejects_magic_suffix_losing_trailing_directory_constraint() {
        reject(
            LOOKUP,
            "remaining.push(b'/')",
            "discard_trailing_slash()",
            "trailing slash",
        );
    }
    #[test]
    fn rejects_magic_target_error_retry() {
        reject(
            LOOKUP,
            "match opened.inode().link_target()?",
            "match opened.inode().link_target().unwrap()",
            "without retry",
        );
    }
    #[test]
    fn rejects_projection_without_namespace_guard() {
        reject(
            "kernel/src/fs/vfs/namespace_access.rs",
            "_guard: TaskMutexGuard<'a, ()>",
            "_guard: ()",
            "namespace capability",
        );
    }
    #[test]
    fn rejects_proc_metadata_recursive_projection() {
        reject(
            "kernel/src/fs/procfs.rs",
            "size: node.metadata_size(),",
            "size: self.read_link()?.len() as u64,",
            "metadata must not reacquire",
        );
    }
}
