use quote::ToTokens;
use syn::Item;

use super::SourceFile;

const ADDRESS_SPACE: &str = "kernel/src/task/model/address_space.rs";
const TASK_ACCESS: &str = "kernel/src/task/model/address_space/task_access.rs";
const EXIT: &str = "kernel/src/task/task_manager/process_exit.rs";
const MUTEX: &str = "kernel/src/sync/task_mutex.rs";

pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let Some(address_space) = source(sources, ADDRESS_SPACE, errors) else {
        return;
    };
    require_field(
        address_space,
        "AddressSpace",
        "memory_set",
        "TaskMutex < MemorySet >",
        errors,
    );
    require_field(
        address_space,
        "AddressSpace",
        "token",
        "crate :: arch :: mmu :: AddressSpaceToken",
        errors,
    );
    for required in [
        ".lock_prepared(wait)",
        ".try_lock()",
        "reclaim_private_pages",
    ] {
        if !address_space.text.contains(required) {
            errors.push(format!(
                "{ADDRESS_SPACE}: address-space blocking owner lost `{required}`"
            ));
        }
    }

    let Some(task_access) = source(sources, TASK_ACCESS, errors) else {
        return;
    };
    if task_access.text.contains("memory_set.lock().token()")
        || !task_access
            .text
            .contains("self.process.address_space().token")
    {
        errors.push(format!(
            "{TASK_ACCESS}: IRQ-disabled trap return must read the immutable token without locking"
        ));
    }

    let Some(mutex) = source(sources, MUTEX, errors) else {
        return;
    };
    for required in [
        "struct TaskMutexWaitPreparation",
        "fn lock_prepared",
        "preparation.disarm(waiter)",
    ] {
        if !mutex.text.contains(required) {
            errors.push(format!(
                "{MUTEX}: post-commit blocking acquisition lost `{required}`"
            ));
        }
    }
    let fast_path = mutex.text.find("if let Some(guard) = self.try_lock()");
    let waiter_allocation = mutex
        .text
        .find("let mut preparation = TaskMutexWaitPreparation::prepare()?");
    if !matches!((fast_path, waiter_allocation), (Some(fast), Some(allocation)) if fast < allocation)
    {
        errors.push(format!(
            "{MUTEX}: uncontended acquisition must perform zero waiter allocations"
        ));
    }

    let Some(exit) = source(sources, EXIT, errors) else {
        return;
    };
    let retire = exit.text.find("task.remove_thread_trap_context();");
    let detach = exit
        .text
        .find("take_current_task().expect(\"exiting task lost current ownership\")");
    if !matches!((retire, detach), (Some(retire), Some(detach)) if retire < detach) {
        errors.push(format!(
            "{EXIT}: blocking memory retirement must complete before current-task detach"
        ));
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
            errors.push(format!("{path}: missing address-space blocking-lock owner"));
            None
        })
}

fn require_field(
    source: &SourceFile,
    structure: &str,
    field: &str,
    expected: &str,
    errors: &mut Vec<String>,
) {
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
            "{}: {structure}.{field} must be `{expected}`, found {actual:?}",
            source.relative
        ));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn repository_blocks_address_space_waiters_without_spinning() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:#?}");
    }
}
