use quote::ToTokens;
use syn::Item;

use super::SourceFile;

const EXT2: &str = "kernel/src/fs/ext2.rs";
const JOURNAL: &str = "kernel/src/fs/ext2/journal.rs";
const INODE_MUTATION: &str = "kernel/src/fs/ext2/journal/inode_mutation.rs";
const MOUNT: &str = "kernel/src/fs/ext2/mount.rs";
const PAGE_CACHE: &str = "kernel/src/fs/page_cache.rs";
const VFS: &str = "kernel/src/fs/vfs.rs";
const MUTEX: &str = "kernel/src/sync/task_mutex.rs";
const ADAPTER: &str = "kernel/src/task/task_manager/task_mutex_wait.rs";

pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    require_field(
        sources,
        EXT2,
        "Ext2FileSystem",
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
    for required in [
        "prepare_current_block",
        "WaitMembership::TaskMutex(key)",
        "assert!(crate::task::processor::wake_waiting_task",
    ] {
        if !adapter.text.contains(required) {
            errors.push(format!(
                "{ADAPTER}: task mutex scheduler adapter lost `{required}`"
            ));
        }
    }
}

fn require_mount_io_snapshot(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(mount) = source(sources, MOUNT, errors) else {
        return;
    };
    for required in [
        "let group_count = self.groups.lock().len();",
        "for i in 0..group_count",
        "*groups.get(i).ok_or(FileSystemError::InvalidFileSystem)?",
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
    if journal.text.contains("MutexGuard<'inode, Ext2InodeDisk>")
        || !journal
            .text
            .contains("Result<InodeMutation<'mutation, 'inode>, FileSystemError>")
    {
        errors.push(format!(
            "{JOURNAL}: inode mutation must return a lock-free working copy, never a spin guard"
        ));
    }

    let Some(working_copy) = source(sources, INODE_MUTATION, errors) else {
        return;
    };
    for required in [
        "struct InodeMutation",
        "disk: Ext2InodeDisk",
        "transaction: PhantomData<&'mutation mut ()>",
        "impl Drop for InodeMutation",
        "*self.inode.disk.lock() = self.disk",
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
    fn repository_blocks_filesystem_transaction_waiters_without_spinning() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:#?}");
    }
}
