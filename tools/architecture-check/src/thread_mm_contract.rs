//! Per-Thread mm user 与旧 mm release/exec graph 提交的 ownership 围栏。
use super::SourceFile;
use quote::ToTokens;
use syn::Item;
const MODEL: &str = "kernel/src/task/model.rs";
const MM: &str = "kernel/src/task/model/thread_mm.rs";
const EXIT: &str = "kernel/src/task/model/process_exit.rs";
const EXEC: &str = "kernel/src/task/model/process_exec.rs";

fn source<'a>(sources: &'a [SourceFile], path: &str) -> &'a SourceFile {
    sources
        .iter()
        .find(|source| source.relative == path)
        .expect("mm owner source")
}
fn field(source: &SourceFile, structure: &str, name: &str) -> Option<String> {
    source.syntax.items.iter().find_map(|item| {
        let Item::Struct(item) = item else {
            return None;
        };
        if item.ident != structure {
            return None;
        }
        item.fields
            .iter()
            .find(|field| field.ident.as_ref().is_some_and(|id| id == name))
            .map(|field| field.ty.to_token_stream().to_string())
    })
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let model = source(sources, MODEL);
    for (structure, name, expected) in [
        ("ThreadContext", "mm", "MmHandle"),
        ("Process", "leader_mm", "Mutex < MmHandle >"),
        ("TaskControlBlock", "scheduling", "Arc < SchedulingEntity >"),
        (
            "Process",
            "leader_scheduler",
            "Mutex < Arc < SchedulingEntity > >",
        ),
        (
            "ExecutionContext",
            "identity",
            "Mutex < Arc < crate :: namespace :: PidIdentity > >",
        ),
        ("Process", "credential_guard", "TaskMutex < () >"),
        ("Process", "retirements", "Arc < ThreadRetirements >"),
        (
            "ThreadContext",
            "memory_cleanup_wait",
            "Mutex < Option < TaskMutexWaitPreparation > >",
        ),
    ] {
        if field(model, structure, name).as_deref() != Some(expected) {
            errors.push(format!(
                "{MODEL}: {structure}.{name} must retain canonical owner `{expected}`"
            ));
        }
    }
    if field(model, "Process", "address_space").is_some() {
        errors.push(format!(
            "{MODEL}: Process cannot independently own a mutable mm root"
        ));
    }
    let kernel = source(sources, "kernel/src/task/model/kernel_thread.rs");
    let identity = source(sources, "kernel/src/task/model/thread_identity.rs");
    let graph = source(sources, "kernel/src/task/process_table.rs");
    let kernel_exit = source(sources, "kernel/src/task/process_table/process_exit.rs");
    if field(kernel, "KernelThread", "pid").as_deref()
        != Some("Mutex < Option < PidRegistration > >")
        || !kernel.text.contains("self.pid.lock().take()")
        || !identity
            .text
            .contains("TaskKind::Kernel(thread) => thread.identity()")
        || !graph
            .text
            .contains(".allocate_pid(crate::namespace::initial_pid(), &[], &authority)")
        || !graph.text.contains("pid.publish()")
        || !graph
            .text
            .split_whitespace()
            .collect::<String>()
            .contains("current_task().map_or_else(crate::namespace::idle_pid_identity")
        || !kernel_exit.text.contains(".retire_kernel_pid()")
    {
        errors.push("kernel-pid: actual executor must own canonical initial PID registration and explicitly retire it before Processor current removal; observer pins cannot own occupancy".into());
    }
    let mm = source(sources, MM);
    let alias = mm.syntax.items.iter().find_map(|item| match item {
        Item::Type(item) if item.ident == "MmHandle" => Some(item.ty.to_token_stream().to_string()),
        _ => None,
    });
    if alias.as_deref() != Some("Arc < Mutex < MmMembership > >")
        || !mm.text.contains(".retire()")
        || mm.text.contains("mm_users: Atomic")
    {
        errors.push(format!(
            "{MM}: independent Thread handle must retire its sole mapping user and retain only authorization facts"
        ));
    }
    let membership = source(sources, "kernel/src/task/model/thread_mm/membership.rs");
    let membership: String = membership.text.split_whitespace().collect();
    if !membership
        .contains("letuser_dumpable=self.live().expect(\"Threadmmretiredtwice\").dumpable();")
        || !membership.contains("core::mem::replace(self,Self::Retired{user_dumpable})")
        || !membership.contains("Self::Live(mm)=>mm")
    {
        errors.push("thread-mm: retirement must freeze native dumpability before moving the actual root, without another mm user".into());
    }
    let exit = source(sources, EXIT);
    if !exit
        .text
        .contains("address_space: self.take_address_space(),")
        || !exit
            .text
            .contains("Arc::strong_count(&self.address_space) == 1")
        || exit.text.contains("address_space: self.address_space()")
    {
        errors.push(format!(
            "{EXIT}: release must move one old mm user and skip clearTid when it is last"
        ));
    }
    if field(exit, "UserExitMemory", "wait").as_deref() != Some("TaskMutexWaitPreparation")
        || !exit.text.contains("lock_prepared(wait)")
        || !exit.text.contains("&mut self.wait,")
        || exit.text.contains(".is_err()")
        || !exit.text.contains("wait: self.take_memory_cleanup_wait()")
        || !exit.text.contains("task.restore_memory_cleanup_wait(wait)")
    {
        errors.push(format!("{EXIT}: prepared mm release must attempt wake independently of write fault and return exec waiter"));
    }
    let exec = source(sources, EXEC);
    let exec_compact: String = exec.text.split_whitespace().collect();
    let ordering = [
        "ExecPreparation::claim(self)",
        "self.prepare_exec_credentials",
        "loaded.build_address_space",
        "preparation.terminate_siblings(self)",
        ".prepare_file_table_unshare()",
        ".prepare_exec_signal_handlers()",
        "self.cleanup_robust_list();",
    ];
    let mut last = 0;
    for stage in ordering {
        match exec_compact.find(&stage.split_whitespace().collect::<String>()) {
            Some(position) if position > last => last = position,
            _ => errors.push(format!("{EXEC}: exec credentials and latest files/sighand must serialize around complete sibling retirement")),
        }
    }
    let tsync = source(sources, "kernel/src/task/process_table/seccomp.rs");
    if !tsync.text.contains(".credential_guard()") || !tsync.text.contains(".lock_killable()") {
        errors.push(
            "exec: TSYNC must hold the same killable Process credential preparation guard".into(),
        );
    }
    let drain = source(sources, "kernel/src/task/process_table/exec_drain.rs");
    if !drain
        .text
        .contains("thread_retirements().wait_for_survivor()")
        || !drain
            .text
            .contains("Arc::ptr_eq(&state.drain, &self.drain)")
        || drain.text.contains("remaining.store(")
    {
        errors.push("exec: drain must own exact identity, concurrent birth and post-cleanup retirement with fatal-only cancellation".into());
    }
    let retirement = source(sources, "kernel/src/task/model/thread_retirement.rs");
    let graph = source(sources, "kernel/src/task/process_table.rs");
    if !retirement.text.contains("completed.wait_killable()")
        || !retirement.text.contains("outstanding.fetch_add(1")
        || !retirement.text.contains("outstanding.fetch_sub(1")
        || !graph
            .text
            .contains("creator.thread_retirements().publish_thread();")
    {
        errors.push(
            "exec: persistent Thread lifecycle must include pre-drain removed MM/vfork cleanup"
                .into(),
        );
    }
    let graph_exit = source(sources, "kernel/src/task/process_table/process_exit.rs");
    if !matches!((graph_exit.text.find("exit_memory.complete();"), graph_exit.text.find("thread_retirement.complete();")), (Some(a), Some(b)) if a < b)
        || !matches!((graph_exit.text.find("if let Some(completion) = vfork_completion"), graph_exit.text.find("thread_retirement.complete();")), (Some(a), Some(b)) if a < b)
        || !graph_exit
            .text
            .contains("preserve_pending_fatal && current.has_fatal_signal()")
    {
        errors.push("exec: drain completion must follow MM cleanup and PONR failure preserve pending SIGKILL".into());
    }
    let access = source(
        sources,
        "kernel/src/task/model/address_space/task_access.rs",
    );
    for name in [
        "copy_from_user",
        "copy_from_user_uninit",
        "copy_instruction_halfword",
        "copy_to_user",
        "zero_user",
        "validate_user_write",
        "copy_user_c_string",
    ] {
        let body = access.syntax.items.iter().find_map(|item| {
            let Item::Impl(item) = item else {
                return None;
            };
            item.items.iter().find_map(|item| {
                let syn::ImplItem::Fn(method) = item else {
                    return None;
                };
                (method.sig.ident == name).then(|| method.block.to_token_stream().to_string())
            })
        });
        if body.is_none_or(|body| {
            body.contains(". address_space (")
                || !body.contains(". mm_snapshot () . ok_or (UserAccessError :: Fault) ?")
        }) {
            errors.push(format!("{name}: fallible user-copy must acquire actual mmget and return Fault after Thread MM retirement"));
        }
    }
    let stages = [
        "self.cleanup_robust_list();",
        "mark_process_exec(self, exec_pid_link, exec_trace_link)",
        "self.replace_address_space(new_address_space)",
        "release_exec_mm(",
        "complete_vfork_exec(self.tid())",
    ];
    let mut previous = 0;
    for stage in stages {
        match exec_compact.find(&stage.split_whitespace().collect::<String>()) {
            Some(position) if position > previous => previous = position,
            _ => errors.push(format!("{EXEC}: robust old TID, promotion, mm replacement, old release and VFORK wake must preserve order")),
        }
    }
    for required in [
        "self.take_clear_child_tid()",
        "self.thread().mm.clone()",
        "self.scheduling.clone()",
    ] {
        if !exec.text.contains(required) {
            errors.push(format!("{EXEC}: exec must consume old registration and retarget readonly aliases `{required}`"));
        }
    }
}
#[cfg(test)]
mod tests {
    fn reject(path: &str, old: &str, new: &str, message: &str) {
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
            errors.iter().any(|error| error.contains(message)),
            "{errors:#?}"
        );
    }
    #[test]
    fn production_owns_one_thread_mapping_user() {
        let sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let mut errors = Vec::new();
        super::check(&sources, &mut errors);
        assert!(errors.is_empty(), "{errors:#?}");
    }
    #[test]
    fn rejects_tcb_observer_used_as_live_mm_proof() {
        reject(
            "kernel/src/task/model/address_space/task_access.rs",
            "self.mm_snapshot()\n            .ok_or(UserAccessError::Fault)?",
            "self.address_space()",
            "fallible user-copy",
        );
    }
    #[test]
    fn rejects_kernel_actor_accessing_user_process() {
        reject(
            "kernel/src/task/model/thread_identity.rs",
            "TaskKind::Kernel(thread) => thread.identity()",
            "TaskKind::Kernel(_) => self.process().tgid.identity()",
            "kernel-pid: actual executor",
        );
    }
    #[test]
    fn rejects_kernel_pid_retirement_delayed_until_tcb_observer_drop() {
        reject(
            "kernel/src/task/process_table/process_exit.rs",
            ".retire_kernel_pid()",
            ".tid()",
            "kernel-pid: actual executor",
        );
    }
    #[test]
    fn rejects_process_owned_second_root() {
        reject(
            super::MODEL,
            "leader_mm: Mutex<MmHandle>,",
            "leader_mm: Mutex<MmHandle>, address_space: Mutex<Arc<AddressSpace>>,",
            "cannot independently own",
        );
    }
    #[test]
    fn rejects_exit_observer_clone_inflating_users() {
        reject(
            super::EXIT,
            "address_space: self.take_address_space(),",
            "address_space: self.address_space(),",
            "move one old mm user",
        );
    }
    #[test]
    fn rejects_last_user_clear_tid() {
        reject(
            super::EXIT,
            "Arc::strong_count(&self.address_space) == 1",
            "false",
            "skip clearTid",
        );
    }
    #[test]
    fn rejects_old_registration_surviving_exec() {
        reject(
            super::EXEC,
            "self.take_clear_child_tid(),",
            "None,",
            "consume old registration",
        );
    }
    #[test]
    fn rejects_promotion_before_robust_old_tid_cleanup() {
        reject(
            super::EXEC,
            "self.cleanup_robust_list();",
            "",
            "preserve order",
        );
    }
    #[test]
    fn rejects_inline_scheduler_mirrored_for_proc() {
        reject(
            super::MODEL,
            "pub(crate) scheduling: Arc<SchedulingEntity>,",
            "pub(crate) scheduling: SchedulingEntity,",
            "canonical owner",
        );
    }
    #[test]
    fn rejects_write_fault_suppressing_wake() {
        reject(
            super::EXIT,
            "let key = memory.futex_key",
            "if memory.copy_to_user(address, &[], self.fault_limits).is_err() { return Err(UserAccessError::Fault); } let key = memory.futex_key",
            "independently of write fault",
        );
    }
    #[test]
    fn rejects_exit_allocating_a_contended_waiter() {
        reject(
            super::EXIT,
            ".lock_prepared(wait)",
            ".lock().unwrap()",
            "prepared mm release",
        );
    }
    #[test]
    fn rejects_exec_losing_the_reusable_cleanup_waiter() {
        reject(
            super::EXIT,
            "task.restore_memory_cleanup_wait(wait);",
            "drop(wait);",
            "return exec waiter",
        );
    }
    #[test]
    fn rejects_files_snapshot_before_sibling_retirement() {
        reject(
            super::EXEC,
            "preparation.terminate_siblings(self)?;",
            "",
            "latest files/sighand",
        );
    }
    #[test]
    fn rejects_tsync_without_exec_credential_guard() {
        reject(
            "kernel/src/task/process_table/seccomp.rs",
            ".lock_killable()",
            ".lock()",
            "same killable",
        );
    }
    #[test]
    fn rejects_ordinary_signal_cancelling_exec_after_ponr() {
        reject(
            "kernel/src/task/model/thread_retirement.rs",
            "completed.wait_killable()",
            "completed.wait_interruptible()",
            "persistent Thread lifecycle",
        );
    }
    #[test]
    fn rejects_post_ponr_oom_overriding_pending_sigkill() {
        reject(
            "kernel/src/task/process_table/process_exit.rs",
            "preserve_pending_fatal && current.has_fatal_signal()",
            "false",
            "preserve pending SIGKILL",
        );
    }
    #[test]
    fn rejects_exec_retirement_before_original_vfork_completion() {
        reject(
            "kernel/src/task/process_table/process_exit.rs",
            "let process_completed = thread_retirement.complete();",
            "let process_completed = true;",
            "completion must follow MM cleanup",
        );
    }
}
