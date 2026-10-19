//! Anonymous identity、pipe session、volatile times 和 seal transaction 的 owner 围栏。
use super::SourceFile;
use quote::ToTokens;
fn text<'a>(sources: &'a [SourceFile], path: &str) -> &'a str {
    &sources
        .iter()
        .find(|s| s.relative == path)
        .expect("anonymous owner source")
        .text
}
fn ordered(body: &str, facts: &[&str]) -> bool {
    let mut rest = body;
    for fact in facts {
        let Some(at) = rest.find(fact) else {
            return false;
        };
        rest = &rest[at + fact.len()..];
    }
    true
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let ofd = text(sources, "kernel/src/file/mod.rs");
    let snapshot = text(sources, "kernel/src/fs/procfs/snapshot.rs");
    let link = text(sources, "kernel/src/fs/procfs/link.rs");
    let producer = text(sources, "kernel/src/task/model/file_table.rs");
    if !ofd.contains("opened: Arc<OpenedFile>")
        || ofd.contains("character_opened")
        || ofd.contains("MemFile(")
        || snapshot.contains("ProcFileTarget")
        || !snapshot.contains("target: Arc<super::super::OpenedFile>")
        || !producer.contains("target: ofd.opened_ref(),")
        || producer.contains("proc_target")
        || !link.contains("LinkTarget::Opened(entry.target)")
    {
        errors.push("anonymous-inode: every OFD/proc target must retain one actual opened identity without label fallback".into());
    }
    let opened = text(sources, "kernel/src/fs/vfs/opened.rs");
    let dentry = text(sources, "kernel/src/fs/vfs/dentry.rs");
    if !dentry.contains("Anonymous(Vec<u8>)")
        || !ordered(
            opened,
            &[
                "fn walk_path(",
                "self.dentry.diagnostic_label()",
                "return copy_bytes(label)",
                "let mut mount = self.mount.clone()",
            ],
        )
        || !ordered(
            dentry,
            &[
                "impl Drop for Dentry",
                "Place::Named(location)",
                "return;",
                "location.get_mut()",
            ],
        )
        || !dentry.contains("Place::Anonymous(_) => None")
        || opened.contains("OpenedPlace")
    {
        errors.push("anonymous-inode: display/location must bypass namespace registration projection and deletion cleanup".into());
    }
    let anonymous = text(sources, "kernel/src/fs/anonymous.rs");
    let fields = sources
        .iter()
        .find(|s| s.relative == "kernel/src/fs/anonymous.rs")
        .unwrap()
        .syntax
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Struct(item) => Some(item.fields.to_token_stream().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !anonymous.contains("Mutex<Weak<Pipe>>")
        || fields
            .iter()
            .any(|field| field.contains("Arc < Pipe >") || field.contains("Arc < Socket >"))
        || anonymous.contains("InodeOpen::Storage")
        || !anonymous.contains("Err(FileSystemError::NotSupported)")
        || !anonymous.contains("mode: 0o600,")
        || !anonymous.contains("inode: 1,")
        || !anonymous.contains("!matches!(self, Self::Anonymous)")
    {
        errors.push("anonymous-inode: pseudo inode must not pin endpoints, reopen singleton or invent normal type/private notification facts".into());
    }
    let ipc = text(sources, "kernel/src/ipc/pipe.rs");
    let admission = text(sources, "kernel/src/ipc/pipe/pipe_open.rs");
    if !ordered(
        admission,
        &[
            "let mut state = pipe.state.lock()",
            "PipeSession::Retired",
            "require_reader && state.readers == 0",
            "let prior = PipeOpenSnapshot",
            "state.readers += 1",
            "*end.registered.get_mut() = true",
        ],
    ) || !ordered(
        ipc,
        &[
            "fn close(",
            "self.state.lock()",
            "state.session = PipeSession::Retired",
            "core::mem::take(&mut state.bytes)",
            "drop(retired)",
        ],
    ) || !ipc.contains("if self.registered.swap(false, Ordering::AcqRel)")
    {
        errors.push("anonymous-inode: endpoint admission peer facts and last-close retirement must share one state transaction".into());
    }
    let fifo = text(sources, "kernel/src/fs/fifo.rs");
    if !fifo.contains("static FIFOS: TaskMutex<Vec<Slot>>")
        || !ordered(
            fifo,
            &[
                "pub(super) fn open_session",
                "Arc::<FifoFile>::try_new_uninit()",
                "pipe.open(request)",
                "let prior = opened.prior",
                "if kind == PipeOpenKind::Named",
            ],
        )
        || !fifo.contains("&& access == FifoAccess::Read\n")
        || !fifo.contains("&& nonblocking\n")
        || !fifo.contains("&& prior.writers == 0")
    {
        errors.push("anonymous-inode: FIFO must prepare outside spin locks and restrict HUP suppression to no-writer nonblocking named readers".into());
    }
    let memory = text(sources, "kernel/src/fs/memory_file.rs");
    let resize = memory
        .split("pub(super) fn truncate_with_metadata")
        .nth(1)
        .unwrap_or("");
    let resize = resize
        .split("pub(super) fn allocate_with_metadata")
        .next()
        .unwrap();
    let sealing = memory
        .split("pub(crate) fn add_seals")
        .nth(1)
        .unwrap_or("")
        .split("pub(crate) fn read")
        .next()
        .unwrap();
    if !matches!((resize.find("state.pages.truncate(size)"), resize.find("commit(&mut metadata)")), (Some(data), Some(commit)) if data < commit)
        || !ordered(
            resize,
            &[
                "write_sequence",
                "check_truncate",
                "let mut metadata = prepare()?",
                "state.pages.truncate(size)",
                "commit(&mut metadata)",
                "drop(metadata)",
                "invalidate_shared_file",
            ],
        )
        || !ordered(
            sealing,
            &["write_sequence", "self.state()?.seals.add(seals)"],
        )
    {
        errors.push("anonymous-inode: seal rejection/data failure must precede metadata commit and all guards must retire before MM invalidation".into());
    }
    let times = text(sources, "kernel/src/fs/inode_times.rs");
    let metadata = text(sources, "kernel/src/fs/anonymous/metadata.rs");
    let tmp = text(sources, "kernel/src/fs/tmpfs.rs");
    if !memory.contains("times: Arc<super::inode_times::InodeTimes>")
        || memory.contains("modified_ns")
        || memory.contains("modified_seconds:")
        || !metadata.contains("times: TimeOwner")
        || metadata.contains(".max(")
        || tmp.contains(".max(modified)")
        || !tmp.contains("TimeOwner::Memory(file.times())")
        || !times.contains("self.mtime = now;")
        || !times.contains("self.ctime = now;")
        || !times.contains("times.ctime = now;")
    {
        errors.push("anonymous-inode: volatile times must have one shared tuple with content touch and explicit mtime distinct from ctime".into());
    }
    let seals = text(sources, "kernel/src/syscall/fs/fcntl.rs");
    let seals = seals
        .split("F_ADD_SEALS | F_GET_SEALS =>")
        .nth(1)
        .unwrap_or("");
    if !ordered(
        seals,
        &[
            "DataBacking::Memory",
            "command == F_ADD_SEALS && ofd.status_flags() & O_ACCMODE == O_RDONLY",
            "return -errno::EPERM",
            "u32::try_from(argument)",
        ],
    ) || !text(sources, "kernel/src/syscall/fs/open.rs")
        .contains("inode.open_backend(flags, &identity, &task.namespace_authority())")
        || !text(sources, "kernel/src/syscall/memory.rs")
            .contains("matches!(ofd.kind, OpenFileKind::Inode)")
    {
        errors.push("anonymous-inode: actual inode operations and writable seal admission must precede generic memory/mask handling".into());
    }
    let sequential = text(sources, "kernel/src/syscall/fs/io/sequential.rs");
    if !ofd.contains("self.notify(if writable")
        || sequential
            .matches("ofd.notify(crate::fs::IN_ACCESS)")
            .count()
            != 2
        || sequential
            .matches("ofd.notify(crate::fs::IN_MODIFY)")
            .count()
            != 2
        || !text(sources, "kernel/src/fs/inotify.rs").contains("!inode.allows_notifications()")
    {
        errors.push("anonymous-inode: generic successful I/O and final close must notify actual non-private inode identities".into());
    }
}
#[cfg(test)]
#[path = "anonymous_inode_contract_tests.rs"]
mod tests;
