use quote::ToTokens;
use syn::Item;

use super::SourceFile;

const EXT4: &str = "kernel/src/fs/ext4.rs";
const JOURNAL: &str = "kernel/src/fs/ext4/journal.rs";
const INODE: &str = "kernel/src/fs/ext4/inode.rs";
const INODE_MUTATION: &str = "kernel/src/fs/ext4/journal/inode_mutation.rs";
const MOUNT: &str = "kernel/src/fs/ext4/mount.rs";
const PAGE_CACHE: &str = "kernel/src/fs/page_cache.rs";
const VFS: &str = "kernel/src/fs/vfs.rs";
const MUTEX: &str = "kernel/src/sync/task_mutex.rs";
const ADAPTER: &str = "kernel/src/task/process_table/task_wait.rs";
const EVENT: &str = "kernel/src/sync/task_event.rs";

pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    require_field(
        sources,
        EXT4,
        "Ext4FileSystem",
        "mutation",
        "TaskMutex < () >",
        errors,
    );
    require_field(
        sources,
        JOURNAL,
        "MutationGuard",
        "_lock",
        "TaskMutexGuard < 'a , () >",
        errors,
    );
    require_inode_working_copy(sources, errors);
    require_mount_io_snapshot(sources, errors);
    require_field(
        sources,
        PAGE_CACHE,
        "CachedFile",
        "operation",
        "TaskMutex < () >",
        errors,
    );
    require_field(
        sources,
        PAGE_CACHE,
        "CachedFile",
        "write_sequence",
        "TaskMutex < () >",
        errors,
    );
    require_field(
        sources,
        VFS,
        "VirtualFileSystem",
        "namespace_mutation",
        "TaskMutex < () >",
        errors,
    );
    require_field(
        sources,
        "kernel/src/fs/vfs/dentry.rs",
        "Dentry",
        "lock",
        "TaskRwSemaphore",
        errors,
    );
    require_field(
        sources,
        "kernel/src/fs/vfs/mounted_filesystem.rs",
        "MountedFilesystem",
        "rename",
        "TaskMutex < () >",
        errors,
    );

    if let Some(position) = source(sources, "kernel/src/file/position.rs", errors)
        && (!position
            .text
            .contains("struct FilePosition(TaskMutex<u64>)")
            || !position.text.contains("Result<R, PositionError>")
            || position.text.contains("spin::Mutex"))
    {
        errors.push(
            "OFD position must use fallible sleeping transaction ownership across I/O".into(),
        );
    }

    let Some(mutex) = source(sources, MUTEX, errors) else {
        return;
    };
    for required in [
        "enum Ownership",
        "Handoff(u64)",
        "waiter.wait()",
        "waiter.and_then(Waiter::publish)",
    ] {
        if !mutex.text.contains(required) {
            errors.push(format!(
                "{MUTEX}: task mutex must retain blocking FIFO handoff contract `{required}`"
            ));
        }
    }
    if mutex.text.contains("yield_current") || mutex.text.contains("spin_loop()") {
        errors.push(format!(
            "{MUTEX}: contended task mutex may block only through scheduler membership"
        ));
    }

    let Some(adapter) = source(sources, ADAPTER, errors) else {
        return;
    };
    for required in ["prepare_current_block", "WaitMembership::TaskWait(key)"] {
        if !adapter.text.contains(required) {
            errors.push(format!(
                "{ADAPTER}: task wait scheduler adapter lost `{required}`"
            ));
        }
    }
    let compact: String = adapter.text.split_whitespace().collect();
    // 可中断 event 的迟到 wake 可以合法失败；mutex 的 exact wake 仍必须 fail-stop。
    // 围栏跟随显式 policy 及 publication 后的信号复查，不能继续要求无条件 assert。
    for required in [
        "letwoken=crate::task::processor::wake_waiting_task(",
        "assert!(woken||key.interruptible(),",
        "elseifkey.signal_pending(self.as_ref())",
    ] {
        if !compact.contains(required) {
            errors.push(format!(
                "{ADAPTER}: task wait cancellation lost `{required}`"
            ));
        }
    }
    let Some(event) = source(sources, EVENT, errors) else {
        return;
    };
    let compact: String = event.text.split_whitespace().collect();
    if !compact.contains("staticEVENT_TICKETS:AtomicU64=AtomicU64::new(0);")
        || !compact.contains("ticket:EVENT_TICKETS.try_update(Ordering::Relaxed,Ordering::Relaxed,|ticket|{ticket.checked_add(1)})")
    {
        errors.push(format!("{EVENT}: event wait ticket must not repeat across same-address object lifetimes"));
    }
}

fn require_mount_io_snapshot(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(mount) = source(sources, MOUNT, errors) else {
        return;
    };
    for required in [
        "let group_count = self.groups.lock().len();",
        "for group in 0..group_count",
        "let descriptor = self.groups.lock()[group];",
    ] {
        if !mount.text.contains(required) {
            errors.push(format!(
                "{MOUNT}: consistency scan must copy group metadata before block I/O; lost `{required}`"
            ));
        }
    }
    if mount
        .text
        .contains("for (i, gd) in groups.iter().enumerate()")
    {
        errors.push(format!(
            "{MOUNT}: consistency scan may not retain the groups spin guard across block I/O"
        ));
    }
}

fn require_inode_working_copy(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(journal) = source(sources, JOURNAL, errors) else {
        return;
    };
    if journal.text.contains("MutexGuard<'inode, Ext4InodeDisk>")
        || !journal
            .text
            .contains("Result<InodeMutation<'mutation, 'inode>, FileSystemError>")
    {
        errors.push(format!(
            "{JOURNAL}: inode mutation must return a lock-free working copy, never a spin guard"
        ));
    }

    // Working-copy and abort publish only disk bytes: the live swap lease shares the lock,
    // but must never be overwritten by an older journal preimage.
    require_field(
        sources,
        INODE,
        "Ext4Inode",
        "disk",
        "Mutex < LiveInode >",
        errors,
    );
    require_field(sources, INODE, "LiveInode", "disk", "Ext4InodeDisk", errors);
    require_field(sources, INODE, "LiveInode", "swap", "SwapFilePhase", errors);
    if !journal.text.contains("inode.disk.lock().disk = *disk;") {
        errors.push(format!(
            "{JOURNAL}: abort must restore only disk image, preserving live inode lease"
        ));
    }
    let Some(working_copy) = source(sources, INODE_MUTATION, errors) else {
        return;
    };
    for required in [
        "struct InodeMutation",
        "disk: Ext4InodeDisk",
        "transaction: PhantomData<&'mutation mut ()>",
        "impl Drop for InodeMutation",
        "self.inode.disk.lock().disk = self.disk",
    ] {
        if !working_copy.text.contains(required) {
            errors.push(format!(
                "{INODE_MUTATION}: inode working-copy publication lost `{required}`"
            ));
        }
    }
}

fn source<'a>(
    sources: &'a [SourceFile],
    path: &str,
    errors: &mut Vec<String>,
) -> Option<&'a SourceFile> {
    sources
        .iter()
        .find(|source| source.relative == path)
        .or_else(|| {
            errors.push(format!("{path}: missing filesystem blocking-lock owner"));
            None
        })
}

fn require_field(
    sources: &[SourceFile],
    path: &str,
    structure: &str,
    field: &str,
    expected: &str,
    errors: &mut Vec<String>,
) {
    let Some(source) = source(sources, path, errors) else {
        return;
    };
    let actual = source.syntax.items.iter().find_map(|item| match item {
        Item::Struct(item) if item.ident == structure => item
            .fields
            .iter()
            .find(|candidate| candidate.ident.as_ref().is_some_and(|name| name == field))
            .map(|candidate| candidate.ty.to_token_stream().to_string()),
        _ => None,
    });
    if actual.as_deref() != Some(expected) {
        errors.push(format!(
            "{path}: {structure}.{field} must be `{expected}`, found {actual:?}"
        ));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rejects_unconditional_stale_wake_success_and_reused_event_ticket() {
        for (path, old, replacement) in [
            (
                super::ADAPTER,
                "woken || key.interruptible()",
                "woken || true",
            ),
            (
                super::ADAPTER,
                "key.signal_pending(self.as_ref())",
                "self.has_deliverable_signal()",
            ),
            (super::EVENT, "ticket: EVENT_TICKETS", "ticket: self.ticket"),
        ] {
            let root = super::super::repository_root();
            let mut sources = super::super::load_sources(&root).expect("repository sources");
            let source = sources
                .iter_mut()
                .find(|source| source.relative == path)
                .unwrap();
            assert!(source.text.contains(old));
            source.text = source.text.replace(old, replacement);
            let mut errors = Vec::new();
            super::check(&sources, &mut errors);
            assert!(
                !errors.is_empty(),
                "invalid event cancellation escaped fence: {path}"
            );
        }
    }
    #[test]
    fn repository_blocks_filesystem_transaction_waiters_without_spinning() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:#?}");
    }
    #[test]
    fn working_copy_and_abort_cannot_replace_live_inode_lease() {
        for (path, old, replacement) in [
            (
                super::INODE_MUTATION,
                "self.inode.disk.lock().disk = self.disk",
                "*self.inode.disk.lock() = self.disk",
            ),
            (
                super::JOURNAL,
                "inode.disk.lock().disk = *disk;",
                "*inode.disk.lock() = *disk;",
            ),
        ] {
            let root = super::super::repository_root();
            let mut sources = super::super::load_sources(&root).expect("repository sources");
            let source = sources
                .iter_mut()
                .find(|source| source.relative == path)
                .unwrap();
            assert!(source.text.contains(old));
            source.text = source.text.replace(old, replacement);
            let mut errors = Vec::new();
            super::check(&sources, &mut errors);
            assert!(
                !errors.is_empty(),
                "lease-overwriting publication escaped the fence: {path}"
            );
        }
    }
}
