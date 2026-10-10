use quote::ToTokens;
use syn::Item;

use super::SourceFile;

const ADDRESS_SPACE: &str = "kernel/src/task/model/address_space.rs";
const TASK_ACCESS: &str = "kernel/src/task/model/address_space/task_access.rs";
const EXIT: &str = "kernel/src/task/process_table/process_exit.rs";
const EXIT_MEMORY: &str = "kernel/src/task/model/process_exit.rs";
const MUTEX: &str = "kernel/src/sync/task_mutex.rs";
const PROCESS_CLONE: &str = "kernel/src/task/model/process_clone.rs";
const COW: &str = "kernel/src/memory/mm/cow.rs";

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
    check_fork_publication(sources, address_space, errors);
    check_mapping_installation(sources, errors);
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
        "crate::memory::reclaim_address_space(&self.memory_set, request)",
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
        || !task_access.text.contains("self.address_space().token")
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
    let Some(exit_memory) = source(sources, EXIT_MEMORY, errors) else {
        return;
    };
    require_field(
        exit_memory,
        "UserExitMemory",
        "address_space",
        "Arc < AddressSpace >",
        errors,
    );
    let exit_body = exit
        .text
        .split("fn prepare_current_exit(")
        .nth(1)
        .unwrap_or("");
    let exit_body: String = exit_body.split_whitespace().collect();
    let prepare = exit_body.find("task.prepare_user_exit_memory()");
    let graph_lock = exit_body.find("super::signal::lock_child_notifications(");
    let completion = exit_body.find("exit_memory.complete();");
    let detach = exit_body.find("take_current_task().expect(\"exitingtasklostcurrentownership\")");
    if !matches!((prepare, graph_lock, completion, detach), (Some(prepare), Some(graph), Some(complete), Some(detach)) if prepare < graph && graph < complete && complete < detach)
    {
        errors.push(format!(
            "{EXIT}: old-mm preparation must precede graph removal and completion must precede current-task detach"
        ));
    }
    let robust = exit_memory.text.find("self.cleanup_robust_list();");
    let retire = exit_memory.text.find("self.remove_thread_trap_context();");
    let token = exit_memory
        .text
        .find("address_space: self.take_address_space(),");
    if !matches!((robust, retire, token), (Some(robust), Some(retire), Some(token)) if robust < retire && retire < token)
    {
        errors.push(format!(
            "{EXIT_MEMORY}: robust/context retirement must complete before publishing exit token"
        ));
    }
    let completion_body = exit_memory
        .text
        .split("fn complete(mut self)")
        .nth(1)
        .unwrap_or("");
    for required in [
        "self.address_space",
        ".copy_to_user(",
        ".futex_key(",
        "lock_prepared(wait)",
        "&mut self.wait,",
    ] {
        if !completion_body.contains(required) {
            errors.push(format!(
                "{EXIT_MEMORY}: old-mm completion lost `{required}`"
            ));
        }
    }
    if completion_body.contains(".process()") {
        errors.push(format!(
            "{EXIT_MEMORY}: post-graph completion must not reread the replaceable Process mm handle"
        ));
    }
    if !exit_memory.text.contains("#[must_use =") {
        errors.push(format!(
            "{EXIT_MEMORY}: exit token must require explicit completion"
        ));
    }
}

fn check_fork_publication(
    sources: &[SourceFile],
    address_space: &SourceFile,
    errors: &mut Vec<String>,
) {
    let body = address_space
        .text
        .split("fn try_clone_for_fork(&self)")
        .nth(1)
        .and_then(|body| body.split("pub(super) fn page_statistics").next())
        .unwrap_or("");
    let registration = body.find("Self::new(MemorySet::try_new()?, self.user_namespace.clone())?");
    let parent_guard = body.find("let mut parent_memory = self");
    let child_guard = body.find("let mut child_memory = child");
    let inherit = body.find("let result = parent_memory.try_clone_for_fork(&mut child_memory);");
    let completion = body.find("crate::memory::complete_memory_mapping_owner_initialization();");
    let propagate = body.find("result?;");
    let result = body.find("Ok(child)");
    if !matches!((registration, parent_guard, child_guard, inherit, completion, propagate, result),
        (Some(register), Some(parent), Some(child), Some(inherit), Some(completion), Some(propagate), Some(result))
        if register < parent && parent < child && child < inherit && inherit < completion && completion < propagate && propagate < result)
        || body.contains("drop(child_memory)")
        || body.contains("drop(parent_memory)")
        || body.contains("*child_memory =")
    {
        errors.push(format!(
            "{ADDRESS_SPACE}: fork must register the empty child before inheriting pages under both mm guards, retaining the original root"
        ));
    }
    let Some(process_clone) = source(sources, PROCESS_CLONE, errors) else {
        return;
    };
    if !process_clone
        .text
        .contains("parent_address_space.try_clone_for_fork()?")
    {
        errors.push(format!(
            "{PROCESS_CLONE}: fork must publish through the registered AddressSpace owner"
        ));
    }
    let Some(cow) = source(sources, COW, errors) else {
        return;
    };
    if !cow.text.contains("cloned: &mut Self") || cow.text.contains("Self::try_new()") {
        errors.push(format!(
            "{COW}: fork must fill the registered child root instead of returning a bare MemorySet"
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

/// actual MM transaction、source pins 与 failure retirement 的组合证明。
fn check_mapping_installation(sources: &[SourceFile], errors: &mut Vec<String>) {
    let paths = [
        "kernel/src/memory/mm/mmap/installation.rs",
        "kernel/src/task/model/address_space/mapping.rs",
        "kernel/src/syscall/memory.rs",
    ];
    let found: Vec<_> = paths
        .iter()
        .filter_map(|path| source(sources, path, errors))
        .collect();
    if found.len() != paths.len() {
        return;
    }
    let install = &found[0].text;
    let mapping = &found[1].text;
    let syscall = found[2]
        .syntax
        .items
        .iter()
        .find_map(|item| match item {
            Item::Fn(function) if function.sig.ident == "sys_mmap" => {
                Some(function.block.to_token_stream().to_string())
            }
            _ => None,
        })
        .unwrap_or_default();
    let preflight = install.find("total > limits.address_space");
    let unmap = install.find("self.prepare_user_unmap(address, length)?");
    let map = install.find("let result = match source");
    if !matches!((preflight, unmap, map), (Some(preflight), Some(unmap), Some(map)) if preflight < unmap && unmap < map)
        || !install
            .split_whitespace()
            .collect::<String>()
            .contains("Ok(PreparedUserMapping{result,retirement})")
        || !install.contains("retirement.synchronize()")
        || !mapping.contains("let retained = request.source.clone()")
        || !mapping.contains("memory.prepare_user_mapping(request, limits)")
        || !mapping.contains("prepared.and_then(|prepared| prepared.synchronize())")
        || !syscall.contains("task . install_user_mapping")
        || syscall.contains("unmap_user_mapping")
    {
        errors.push("mapping-installation: fixed replacement and publication must share actual MM guard, preflight net limits and carry old retirement through success and failure".into());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rejects_fork_registration_after_page_inheritance() {
        let root = super::super::repository_root();
        let mut sources = super::super::load_sources(&root).expect("repository sources");
        let address_space = sources
            .iter_mut()
            .find(|source| source.relative == super::ADDRESS_SPACE)
            .unwrap();
        address_space.text = address_space
            .text
            .replace("let child = Self::new(MemorySet::try_new()?, self.user_namespace.clone())?;", "")
            .replace(
                "let result = parent_memory.try_clone_for_fork(&mut child_memory);",
                "let result = parent_memory.try_clone_for_fork(&mut child_memory);\nlet child = Self::new(MemorySet::try_new()?, self.user_namespace.clone())?;",
            );
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("fork must register the empty child")),
            "{errors:#?}"
        );
    }

    #[test]
    fn rejects_fork_replacing_the_registered_root() {
        let root = super::super::repository_root();
        let mut sources = super::super::load_sources(&root).expect("repository sources");
        let address_space = sources
            .iter_mut()
            .find(|source| source.relative == super::ADDRESS_SPACE)
            .unwrap();
        address_space.text = address_space.text.replace(
            "let result = parent_memory.try_clone_for_fork(&mut child_memory);",
            "*child_memory = MemorySet::try_new()?;\nlet result = parent_memory.try_clone_for_fork(&mut child_memory);",
        );
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("retaining the original root")),
            "{errors:#?}"
        );
    }

    #[test]
    fn repository_blocks_address_space_waiters_without_spinning() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).expect("repository sources");
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:#?}");
    }

    #[test]
    fn rejects_context_retirement_after_graph_unregistration() {
        let root = super::super::repository_root();
        let mut sources = super::super::load_sources(&root).expect("repository sources");
        let exit = sources
            .iter_mut()
            .find(|source| source.relative == super::EXIT)
            .unwrap();
        exit.text = exit
            .text
            .replace("let exit_memory = task.prepare_user_exit_memory();", "")
            .replace(
                "exit_memory.complete();",
                "let exit_memory = task.prepare_user_exit_memory();\nexit_memory.complete();",
            );
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("old-mm preparation must precede graph removal")),
            "{errors:#?}"
        );
    }

    #[test]
    fn rejects_completion_through_the_replaceable_process_mm() {
        let root = super::super::repository_root();
        let mut sources = super::super::load_sources(&root).expect("repository sources");
        let exit_memory = sources
            .iter_mut()
            .find(|source| source.relative == super::EXIT_MEMORY)
            .unwrap();
        exit_memory.text = exit_memory.text.replace(
            "fn complete(mut self) -> TaskMutexWaitPreparation {",
            "fn complete(mut self) -> TaskMutexWaitPreparation {\nlet _ = task.process().address_space();",
        );
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("must not reread the replaceable Process mm handle")),
            "{errors:#?}"
        );
    }
}
