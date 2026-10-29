//! trap context publication 的 fallible preparation / no-fail commit 围栏。
use quote::ToTokens;
use syn::Item;

use super::SourceFile;
const MODEL: &str = "kernel/src/execution/task/model.rs";
const CLONE: &str = "kernel/src/execution/task/model/process_clone.rs";
const CONTEXT: &str = "kernel/src/execution/task/model/context_storage.rs";
const EXEC: &str = "kernel/src/execution/task/model/process_exec.rs";

pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    for (path, method) in [(MODEL, "clone_thread"), (CLONE, "clone_process")] {
        let body = method_body(sources, path, method);
        let wait = body.find("TaskMutexWaitPreparation :: prepare");
        let mapping = body.find("allocate_thread_context ()");
        let mm_handle = body.find("try_mm_handle");
        let scheduler = body.find("SchedulingEntity :: try_new");
        if !matches!((wait, mapping), (Some(wait), Some(mapping)) if wait < mapping)
            || body[mapping.unwrap_or(body.len())..].contains("TaskMutexWaitPreparation :: prepare")
        {
            errors.push(format!(
                "{path}: retirement waiter must precede shared context mapping"
            ));
        }
        if !matches!((mm_handle, scheduler, mapping), (Some(mm), Some(scheduler), Some(mapping)) if mm < mapping && scheduler < mapping)
            || body[mapping.unwrap_or(body.len())..].contains("SchedulingEntity :: try_new")
            || body[mapping.unwrap_or(body.len())..].contains("try_mm_handle")
            || body[body.find("let mut child_trap").unwrap_or(body.len())..].contains('?')
        {
            errors.push(format!("{path}: mm/scheduling allocation must precede shared context mapping with no later fallible publication"));
        }
    }
    let allocate = method_body(sources, CONTEXT, "allocate_thread_context");
    if allocate.matches(". lock ()").count() != 1
        || !allocate.contains("let address = memory . allocate_thread_trap_context () ?")
        || !allocate.contains("Self :: user_context_pointer_from (& memory , address)")
        || !allocate
            .contains("ContextOwner :: bind (address , pointer , ContextBacking :: AddressSpace)")
        || allocate.contains("bind_user_context")
    {
        errors.push(format!(
            "{CONTEXT}: shared context allocation and binding require one mm guard"
        ));
    }
    let placement = method_body(
        sources,
        "kernel/src/memory/mm/thread_context.rs",
        "allocate_thread_trap_context",
    );
    if !placement.contains("THREAD_TRAP_CONTEXT_RANGE")
        || !placement.contains("areas . floor")
        || placement.contains("tid")
        || placement.contains("areas . iter")
        || !matches!((placement.find("insert_framed_area"), placement.find("self . thread_context_hint =")),
            (Some(map), Some(hint)) if map < hint)
    {
        errors.push("context placement must use actual ordered VMA gaps with hint commit after successful mapping, independently of lifetime task key".into());
    }
    let exec = method_body(sources, EXEC, "execve_replace");
    if !matches!((exec.find("prepare_context_rebind"), exec.find("mark_process_exec")),
        (Some(prepare), Some(commit)) if prepare < commit)
        || !exec.contains("with_cleanup_memory (& old_address_space")
        || exec.contains(". try_lock ()")
    {
        errors.push(format!(
            "{EXEC}: prepare new context before PONR and retire old shared mm with prepared waiter"
        ));
    }
    let apply = method_body(sources, CONTEXT, "apply");
    if apply.contains(". lock")
        || apply.contains("try_lock")
        || apply.contains("prepare ()")
        || !apply.contains("owner . rebind (self . address , self . pointer)")
    {
        errors.push(format!(
            "{CONTEXT}: context commit must consume prepared pointer without mm acquisition"
        ));
    }
}

fn method_body(sources: &[SourceFile], path: &str, method: &str) -> String {
    sources
        .iter()
        .find(|source| source.relative == path)
        .and_then(|source| {
            source.syntax.items.iter().find_map(|item| {
                let Item::Impl(item) = item else {
                    return None;
                };
                item.items.iter().find_map(|item| match item {
                    syn::ImplItem::Fn(function) if function.sig.ident == method => {
                        Some(function.block.to_token_stream().to_string())
                    }
                    _ => None,
                })
            })
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    fn sources() -> Vec<super::SourceFile> {
        super::super::load_sources(&super::super::repository_root()).unwrap()
    }
    fn reject(path: &str, original: &str, changed: &str, message: &str) {
        let mut sources = sources();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == path)
            .unwrap();
        assert!(source.text.contains(original));
        source.text = source.text.replace(original, changed);
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(
            errors.iter().any(|error| error.contains(message)),
            "{errors:#?}"
        );
    }
    #[test]
    fn production_prepares_context_before_publication() {
        let mut errors = Vec::new();
        super::check(&sources(), &mut errors);
        assert!(errors.is_empty(), "{errors:#?}");
    }
    #[test]
    fn rejects_retirement_waiter_after_mapping() {
        for path in [super::MODEL, super::CLONE] {
            reject(
                path,
                "address_space.allocate_thread_context()?",
                "{ let context = address_space.allocate_thread_context()?; let _ = TaskMutexWaitPreparation::prepare()?; context }",
                "retirement waiter must precede",
            );
        }
    }
    #[test]
    fn rejects_task_id_dependent_context_placement() {
        reject(
            "kernel/src/memory/mm/thread_context.rs",
            "let vpn = VirtualAddress::from(address).floor();",
            "let tid = address / config::PAGE_SIZE; let vpn = VirtualAddress::from(tid * config::PAGE_SIZE).floor();",
            "independently of lifetime task key",
        );
    }
    #[test]
    fn rejects_second_fallible_context_binding() {
        reject(
            super::CONTEXT,
            "let pointer = Self::user_context_pointer_from(&memory, address);",
            "let pointer = self.user_context_pointer(address)?;",
            "one mm guard",
        );
    }
    #[test]
    fn rejects_owner_allocation_after_shared_context_mapping() {
        for path in [super::MODEL, super::CLONE] {
            reject(
                path,
                "let mut child_trap = self.snapshot_user_context_for_clone();",
                "let _ = SchedulingEntity::try_new(policy, affinity, last_cpu)?; let mut child_trap = self.snapshot_user_context_for_clone();",
                "no later fallible publication",
            );
        }
    }
    #[test]
    fn rejects_registered_new_mm_exclusive_assumption() {
        reject(
            super::CONTEXT,
            "unsafe { owner.rebind(self.address, self.pointer) };",
            "let _guard = self.address_space.memory_set.try_lock().unwrap(); unsafe { owner.rebind(self.address, self.pointer) };",
            "without mm acquisition",
        );
    }
    #[test]
    fn rejects_old_shared_mm_exclusive_assumption() {
        reject(
            super::EXEC,
            "self.with_cleanup_memory(&old_address_space, |memory| {",
            "self.with_cleanup_memory(&old_address_space, |memory| { let _ = old_address_space.memory_set.try_lock().unwrap();",
            "retire old shared mm",
        );
    }
}
