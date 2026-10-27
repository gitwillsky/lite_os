//! Sighand table 与 group pending owner 的分离及生命周期围栏。
use super::SourceFile;
use quote::ToTokens;
use syn::Item;
const HANDLERS: &str = "kernel/src/task/model/signal_handlers.rs";
const MODEL: &str = "kernel/src/task/model.rs";
const DELIVERY: &str = "kernel/src/task/model/signal_state.rs";
const EXEC: &str = "kernel/src/task/model/process_exec.rs";
const EXIT: &str = "kernel/src/task/process_table/process_exit.rs";
fn source<'a>(sources: &'a [SourceFile], path: &str) -> &'a SourceFile {
    sources
        .iter()
        .find(|source| source.relative == path)
        .expect("signal owner source")
}
fn field(source: &SourceFile, structure: &str, name: &str) -> Option<String> {
    source.syntax.items.iter().find_map(|item| {
        let Item::Struct(item) = item else {
            return None;
        };
        (item.ident == structure)
            .then(|| {
                item.fields
                    .iter()
                    .find(|field| field.ident.as_ref().is_some_and(|id| id == name))
                    .map(|field| field.ty.to_token_stream().to_string())
            })
            .flatten()
    })
}
fn method(source: &SourceFile, name: &str) -> String {
    source
        .syntax
        .items
        .iter()
        .find_map(|item| {
            let Item::Impl(item) = item else {
                return None;
            };
            item.items.iter().find_map(|item| match item {
                syn::ImplItem::Fn(function) if function.sig.ident == name => {
                    Some(function.block.to_token_stream().to_string())
                }
                _ => None,
            })
        })
        .unwrap_or_default()
}
fn method_of(source: &SourceFile, owner: &str, name: &str) -> String {
    source
        .syntax
        .items
        .iter()
        .find_map(|item| {
            let Item::Impl(item) = item else { return None };
            if item.self_ty.to_token_stream().to_string() != owner {
                return None;
            }
            item.items.iter().find_map(|item| match item {
                syn::ImplItem::Fn(function) if function.sig.ident == name => {
                    Some(function.block.to_token_stream().to_string())
                }
                _ => None,
            })
        })
        .unwrap_or_default()
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    let handlers = source(sources, HANDLERS);
    let signal_file = source(sources, "kernel/src/file/signalfd.rs");
    let ofd = source(sources, "kernel/src/file/mod.rs");
    let epoll = source(sources, "kernel/src/file/epoll.rs");
    let read = source(sources, "kernel/src/syscall/fs/io/sequential/read.rs");
    let wait = source(sources, "kernel/src/syscall/poll/wait_keys.rs");
    if field(signal_file, "SignalFd", "mask").as_deref() != Some("AtomicU64")
        || field(signal_file, "SignalWaitSource", "generation").as_deref() != Some("AtomicU64")
        || signal_file.text.contains("TaskControlBlock")
        || signal_file.text.contains("PendingSignals")
        || !ofd.text.contains("context: &impl FileReadinessContext")
        || !method(handlers, "read_signal_notification").contains("take_pending_signal (mask)")
        || !method(handlers, "read_signal_notification").contains("encode_signalfd (signal)")
        || !read
            .text
            .contains("task.read_signal_notification(signal.mask())")
        || !wait
            .text
            .contains("Signal {\n        source: Arc<crate::file::SignalWaitSource>")
        || !method(epoll, "source_changed")
            .contains("! matches ! (& interest . ofd . kind , OpenFileKind :: SignalFd (_))")
        || !method(epoll, "modify")
            .contains("! matches ! (& interest . ofd . kind , OpenFileKind :: SignalFd (_))")
    {
        errors.push("signalfd: reader-relative canonical dequeue, one mask owner and pinned ADD-time epoll sighand source are mandatory; no queue mirror or notifier-time rebinding".into());
    }
    if field(handlers, "ProcessSignalState", "handlers").as_deref()
        != Some("Option < Arc < Mutex < SignalHandlers > > >")
        || field(handlers, "ProcessSignalState", "actions").is_some()
        || field(handlers, "SignalHandlers", "pending").is_some()
        || field(source(sources, MODEL), "Process", "signal_state").as_deref()
            != Some("Mutex < ProcessSignalState >")
    {
        errors.push(
            "sighand: one retirable shared table; group pending must remain independent".into(),
        );
    }
    let queue = source(sources, "kernel/src/task/model/signal_pending.rs");
    let queue_shape = queue.syntax.to_token_stream().to_string();
    if field(queue, "SignalRecord", "source").as_deref() != Some("SignalRecordSource")
        || !queue_shape.contains("Dynamic { _charge : crate :: namespace :: RlimitCharge")
        || !queue_shape.contains("Timer (Option < Arc < PreallocatedSignal > >)")
        || field(queue, "PreallocatedSignal", "_charge").as_deref()
            != Some("crate :: namespace :: RlimitCharge")
        || field(queue, "SignalRecord", "next").as_deref()
            != Some("Option < Box < SignalRecord > >")
        || field(queue, "PendingSignals", "records").as_deref() != Some("[RetiredSignals ; 64]")
        || field(queue, "RetiredSignals", "head").as_deref()
            != Some("Option < Box < SignalRecord > >")
    {
        errors.push("signal-queue: actual owned FIFO record must distinguish dynamic recipient charge from preallocated timer lifetime; no fixed RT coalescing".into());
    }
    if field(queue, "PendingSignals", "fallback").as_deref() != Some("[SignalFallback ; 65]")
        || field(queue, "PendingSignals", "info").is_some()
        || !source(sources, "kernel/src/task/model/signal_info.rs")
            .text
            .contains("signal < 32 && self.code >= 0")
        || !method_of(queue, "PreparedSignalRecord", "prepare")
            .contains("info . overrides_pending_limit (signal)")
    {
        errors.push("signal-queue: ordinary events use actual charged records and native exemption; bit fallback must not duplicate a payload table".into());
    }
    for name in ["queue", "take", "push", "pop", "append", "discard"] {
        let body = method(queue, name);
        if body.contains("try_new") || body.contains("try_reserve") {
            errors.push("signal-queue: publication/selection/retirement splice must never allocate under owner guard".into());
        }
    }
    let preparation_body = method_of(queue, "PreparedSignalRecord", "prepare");
    if !preparation_body.contains("RlimitCharge :: acquire")
        || !preparation_body.contains("Box :: try_new")
        || !method(queue, "drop").contains("while let Some (record) = self . pop ()")
        || !method(queue, "take").contains("TakenSignal { signal , info")
        || !method(queue, "take").contains("record ,")
        || !method(queue, "discard").contains("core :: mem :: take")
    {
        errors.push("signal-queue: lock-outside preparation, owned consumption and iterative retirement are mandatory".into());
    }
    let dequeue = method(
        source(sources, "kernel/src/task/model/signal_dequeue.rs"),
        "take_pending_signal",
    );
    if !matches!((dequeue.find("drop (pending)"), dequeue.find("drop (state)"), dequeue.find("resolve")),
        (Some(private), Some(shared), Some(retire)) if private < retire && shared < retire)
    {
        errors.push("signal-queue: actual consumed quota/storage must retire after both pending owner guards".into());
    }
    if !source(sources, EXIT)
        .text
        .contains("task.retire_thread_signals();")
        || !handlers
            .text
            .contains("retired.append(pending.discard(bit))")
        || !source(sources, "kernel/src/task/process_table/signal.rs")
            .text
            .contains("drop(graph);")
    {
        errors.push("signal-queue: graph removal and SIG_IGN must retire actual records independently of observer pins".into());
    }
    let signal = source(
        sources,
        "kernel/src/task/process_table/signal/disposition.rs",
    );
    // 顶层 free function 的 graph → timer choreography；动态退休必须在 graph 外。
    if !matches!((signal.text.find("drop(graph);\n    if result.is_ok()"), signal.text.find("result.map(|(action, retired)|")),
        (Some(unlock), Some(retire)) if unlock < retire)
        || !signal.text.contains(
            ".recycle_timers(|event| PROCESS_TABLE.timers.lock().ignore_posix_notification(event))",
        )
    {
        errors.push("signal-queue: SIG_IGN must park and recycle actual live periodic timer nodes before graph unlock; all other charge retirement stays outside graph".into());
    }
    let posix = source(
        sources,
        "kernel/src/task/process_table/timer_queue/posix_schedule.rs",
    );
    let creator = source(
        sources,
        "kernel/src/task/process_table/timer_queue/preparation.rs",
    );
    let timer_owner = method_of(queue, "PreallocatedSignal", "try_new");
    let checkout = method_of(queue, "PreallocatedSignal", "prepare");
    if !timer_owner.contains("RlimitCharge :: acquire")
        || !timer_owner.contains("Box :: try_new")
        || !creator.text.contains("PreallocatedSignal::try_new(")
        || checkout.contains("try_new")
        || checkout.contains("RlimitCharge")
        || !checkout.contains("slot . queued_sequence = sequence")
        || !method(posix, "expire_posix").contains("NotificationPhase :: Publication")
        || method(posix, "expire_posix").contains("next_period")
        || !method(posix, "deliver_posix_notification").contains("Arc :: ptr_eq")
        || !method(posix, "deliver_posix_notification")
            .contains("timer . sequence != event . sequence")
        || !method(posix, "deliver_posix_notification")
            .contains("deadline_index . commit_vacant (deadline)")
        || !method_of(queue, "TakenSignal", "resolve").contains("SignalRecord :: retire")
    {
        errors.push("posix-signal: create owns one precharged reusable record; actual dequeue validates lifetime/sequence and reuses original deadline without fresh allocation".into());
    }
    for name in [
        "expire_posix",
        "prepare_posix_notification",
        "deliver_posix_notification",
        "ignore_posix_notification",
        "resume_ignored_posix",
    ] {
        let body = method(posix, name);
        if body.contains("try_new") || body.contains("try_reserve") {
            errors.push("posix-signal: expiration, ignored transition and consumption must move existing owned storage".into());
        }
    }
    if !method(queue, "discard_timer_notifications").contains("extract_timer_notifications")
        || !source(sources, "kernel/src/task/model/signal_generation.rs")
            .text
            .contains("discard_timer_signals")
        || !source(sources, "kernel/src/task/process_table/timer_queue.rs")
            .text
            .contains("discard_timer_signals(threads.values())")
    {
        errors.push("posix-signal: exec explicitly retires SI_TIMER records while preserving other pending identities".into());
    }
    let preparation = method(handlers, "prepare_exec_signal_handlers");
    let commit = method(handlers, "commit_exec_signal_handlers");
    if preparation.contains(". actions")
        || !commit.contains("detached . actions = original . actions")
        || !commit.contains("detached . reset_for_exec ()")
    {
        errors.push(
            "sighand: preflight reserves destination; latest actions copy/reset belong to commit"
                .into(),
        );
    }
    if !handlers
        .text
        .contains("action.flags &= crate::arch::user::SignalActionBytes::supported_flags()")
    {
        errors.push("sighand: canonical action admission must clear unsupported ABI flags".into());
    }
    let generation = source(sources, "kernel/src/task/model/signal_generation.rs");
    for name in ["queue_signal", "queue_process_signal_mask"] {
        let body = method(generation, name);
        if !matches!((body.find(if name == "queue_signal" { "signal_mask . lock" } else { "recipient_mask . lock" }), body.find("with_handlers")),
            (Some(mask), Some(owner)) if mask < owner)
            || !body.contains("ignores_generation")
        {
            errors.push("sighand: generation must read blocked mask before group/table, with canonical ignored policy".into());
        }
    }
    let retirement = source(
        sources,
        "kernel/src/task/process_table/process_retirement.rs",
    );
    let retirement_shape = retirement.syntax.to_token_stream().to_string();
    for fact in [
        "unlink_consequence (graph , pid , Consequence :: ParentDeath)",
        "Consequence :: ExitEffect (index)",
        ". take_entry (& pid)",
        "self . nodes . commit_vacant",
        "self . parents . commit_vacant",
        "self . creators . commit_vacant",
        "self . groups . commit_vacant",
    ] {
        if !retirement_shape.contains(fact) {
            errors.push("sighand: one retirement must detach consequences/indexes without final Drop under graph".into());
        }
    }
    let graph = source(sources, "kernel/src/task/process_table.rs");
    if field(graph, "ThreadIndex", "vfork_parent").as_deref()
        != Some("Option < Arc < TaskControlBlock > >")
        || field(graph, "ProcessNode", "vfork_parent").is_some()
    {
        errors.push(
            "sighand: vfork completion must belong to original child Thread, not reaped Process"
                .into(),
        );
    }
    let exit = source(sources, EXIT);
    if !matches!((exit.text.find("take_vfork_completion("), exit.text.find("mark_parent_exit("),
        exit.text.find("exit_memory.complete()"), exit.text.find("completion.complete()")),
        (Some(take), Some(remove), Some(cleanup), Some(wake)) if take < remove && cleanup < wake)
    {
        errors.push("sighand: exit must preserve vfork token before index retirement and wake after old-mm cleanup".into());
    }
    let claims = source(sources, "kernel/src/task/process_table/wait_child/claim.rs");
    if claims.text.matches("ExitRetirement::AfterClaim").count() != 2
        || !exit.text.contains("ExitRetirement::AfterClaim")
        || claims.text.contains("publish_child_signal")
    {
        errors.push(
            "sighand: active claim must complete the committed autoreap on consume and release"
                .into(),
        );
    }
    let publication = method(handlers, "publish_child_signal");
    if !publication.contains("handlers . child_policy ()")
        || !publication.contains("pending . queue")
        || !exit.text.contains("stage_child_signal(")
        || exit.text.contains("send_kernel_process_signal(parent")
        || source(
            sources,
            "kernel/src/task/process_table/signal/job_control.rs",
        )
        .text
        .contains("send_kernel_process_signal(")
    {
        errors.push(
            "sighand: SIGCHLD policy and pending must publish together; outside graph only wake"
                .into(),
        );
    }
    let child_routing = source(
        sources,
        "kernel/src/task/process_table/signal/child_notification.rs",
    );
    let child_routing_compact: String = child_routing.text.split_whitespace().collect();
    if !child_routing
        .text
        .contains("let creator = child.parent_thread?")
        || !child_routing_compact
            .contains("recipient.publish_child_signal(info,kind,signal,prepared.record_for(recipient,signal))")
        || child_routing.text.contains("threads.values().next()")
    {
        errors.push("sighand: child generation must use canonical creator/reaper mask, not arbitrary sibling".into());
    }
    let child = source(
        sources,
        "kernel/src/task/process_table/signal/child_notification.rs",
    );
    if !child.text.contains("prepared.prepare();")
        || !child
            .text
            .contains("prepared.matches(&graph, child, scope)")
        || !child
            .text
            .contains("Arc::ptr_eq(&node.process, &actual_child)")
        || !matches!((child.text.find("drop(graph);\n        prepared.prepare();"), child.text.find("prepared.matches(&graph, child, scope)")),
            (Some(prepare), Some(validate)) if prepare < validate)
    {
        errors.push("sighand: SIGCHLD actual parent/adoption recipients prepare outside graph and revalidate same child Process before publication".into());
    }
    let child_compact: String = child.text.split_whitespace().collect();
    let graph = source(sources, "kernel/src/task/process_table.rs");
    let clone = source(sources, "kernel/src/task/process_table/vfork.rs");
    let identity = source(sources, "kernel/src/task/process_table/exec_identity.rs");
    let identity_compact: String = identity.text.split_whitespace().collect();
    let wait = source(sources, "kernel/src/task/process_table/wait_child.rs");
    if field(graph, "ProcessNode", "exit_signal").as_deref() != Some("u8")
        || field(graph, "ProcessNode", "exec_generation").as_deref() != Some("u64")
        || !clone.text.contains("creator_node.exit_signal")
        || !clone.text.contains("creator_node.parent_exec_generation")
        || !identity_compact.contains("node.exec_generation.checked_add(1)")
        || !identity
            .text
            .contains("node.exit_signal = syscall_abi::signal::SIGCHLD as u8")
        || !exit
            .text
            .contains("node.exit_signal = syscall_abi::signal::SIGCHLD as u8")
        || !child_compact.contains("parent.exec_generation!=child.parent_exec_generation")
        || !child_compact.contains("before.signal==after.signal")
        || !publication.contains("autoreap : ordinary && policy . autoreap")
        || !wait.text.contains("!filter.eligible(node, claimant)")
        || !wait.text.contains("node.parent_thread == Some(claimant)")
    {
        errors.push("sighand: native clone exit signal, parent exec epoch, orphan/exec reset and wait class must belong to the actual relation".into());
    }
    let delivery = source(sources, DELIVERY);
    let dequeue = source(sources, "kernel/src/task/model/signal_dequeue.rs");
    let stages = [
        "self.take_pending_signal(!selection_mask)",
        "ptrace::intercept_signal",
        "handlers.select(signal)",
        "self.copy_to_user(frame_address",
    ];
    let mut previous = 0;
    for stage in stages {
        match delivery.text.find(stage) {
            Some(position) if position >= previous => previous = position,
            _ => errors.push(format!("sighand: dequeue, tracer decision, locked one-shot selection and frame copy must preserve order: {stage}")),
        }
    }
    if delivery.text.contains("SA_RESETHAND")
        || delivery.text.contains("state.actions")
        || !dequeue.text.contains("selected.resolve(")
        || !dequeue.text.contains("let (selected, queue) = selected?;")
    {
        errors.push("sighand: timer validation belongs to canonical dequeue; one-shot reset belongs to locked handler selection".into());
    }
    for fact in [
        "self.actions[signal].handler = 0",
        "pending.discard(bit)",
        "thread.thread().pending_signals.lock().discard(bit)",
        "handlers.reset_for_exec()",
        "state.handlers.take()",
        "state.pending.discard(u64::MAX)",
        "drop(retired);",
    ] {
        if !handlers.text.contains(fact) {
            errors.push(format!(
                "sighand: canonical transition/lifetime requires `{fact}`"
            ));
        }
    }
    let exec = source(sources, EXEC);
    if !matches!((exec.text.find("prepare_exec_signal_handlers()"), exec.text.find("self.cleanup_robust_list()"),
        exec.text.find("self.commit_exec_signal_handlers(detached_handlers)")),
        (Some(prepare), Some(ponr), Some(commit)) if prepare < ponr && ponr < commit)
    {
        errors.push(
            "sighand: shared exec detachment must be prepared before PONR and committed once"
                .into(),
        );
    }
    if !source(sources, EXIT)
        .text
        .contains("if process_status.is_some() {\n        task.retire_signal_handlers();")
    {
        errors
            .push("sighand: last Thread exit must retire Process table outside graph guard".into());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Vec<SourceFile> {
        super::super::load_sources(&super::super::repository_root()).unwrap()
    }
    #[test]
    fn accepts_canonical_signal_owners() {
        let mut errors = Vec::new();
        check(&fixture(), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_signalfd_creator_binding() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == "kernel/src/file/signalfd.rs")
            .unwrap();
        source
            .text
            .push_str("\nstruct CreatorBinding { reader: Arc<TaskControlBlock> }\n");
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.iter().any(|error| error.starts_with("signalfd:")));
    }
    #[test]
    fn rejects_signalfd_notifier_rebinding() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == "kernel/src/file/epoll.rs")
            .unwrap();
        source.text = source.text.replace(
            "!matches!(&interest.ofd.kind, OpenFileKind::SignalFd(_))",
            "true",
        );
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.iter().any(|error| error.starts_with("signalfd:")));
    }

    #[test]
    fn rejects_delayed_frame_reset() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == DELIVERY)
            .unwrap();
        source.text.push_str("\n// SA_RESETHAND after user copy\n");
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(!errors.is_empty());
    }
    #[test]
    fn rejects_missing_exit_retirement() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == EXIT)
            .unwrap();
        source.text = source.text.replace("task.retire_signal_handlers();", "");
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(!errors.is_empty());
    }
    #[test]
    fn rejects_exec_allocation_after_ponr() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == EXEC)
            .unwrap();
        source.text = source
            .text
            .replace("prepare_exec_signal_handlers()", "late_detach()");
        source
            .text
            .push_str("\n// prepare_exec_signal_handlers()\n");
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(!errors.is_empty());
    }
    #[test]
    fn rejects_preflight_snapshot_and_missing_latest_copy() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == HANDLERS)
            .unwrap();
        source.text = source.text.replace(
            "detached.actions = original.actions;",
            "detached.actions = old_snapshot;",
        );
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.iter().any(|error| error.contains("latest actions")));
    }
    #[test]
    fn rejects_lost_consequence_unlink_before_retirement() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == "kernel/src/task/process_table/process_retirement.rs")
            .unwrap();
        source.text = source.text.replace(
            "unlink_consequence(graph, pid, Consequence::ParentDeath);",
            "",
        );
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("retirement must detach"))
        );
    }
    #[test]
    fn rejects_arbitrary_parent_sibling_for_child_generation() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| {
                source.relative == "kernel/src/task/process_table/signal/child_notification.rs"
            })
            .unwrap();
        source.text = source
            .text
            .replace("threads.get(&creator)?", "threads.values().next()?");
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.iter().any(|error| error.contains("creator/reaper")));
    }
    #[test]
    fn rejects_vfork_wake_before_memory_cleanup() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == EXIT)
            .unwrap();
        source.text = source
            .text
            .replace("exit_memory.complete();", "cleanup_later();");
        source.text.push_str("\n// exit_memory.complete()\n");
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(errors.iter().any(|error| error.contains("old-mm cleanup")));
    }
    #[test]
    fn rejects_clone_child_autoreap_from_sigchld_policy() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == HANDLERS)
            .unwrap();
        source.text = source.text.replace(
            "autoreap: ordinary && policy.autoreap",
            "autoreap: policy.autoreap",
        );
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("native clone exit signal"))
        );
    }
    #[test]
    fn rejects_prepared_child_signal_after_parent_exec_changes_recipient_signal() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| {
                source.relative == "kernel/src/task/process_table/signal/child_notification.rs"
            })
            .unwrap();
        source.text = source.text.replace("before.signal == after.signal", "true");
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("native clone exit signal"))
        );
    }
    #[test]
    fn rejects_claim_release_that_loses_committed_retirement() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == "kernel/src/task/process_table/wait_child/claim.rs")
            .unwrap();
        source.text =
            source
                .text
                .replacen("ExitRetirement::AfterClaim", "ExitRetirement::Waitable", 1);
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("committed autoreap"))
        );
    }
    #[test]
    fn rejects_child_policy_without_same_lock_pending_publication() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == HANDLERS)
            .unwrap();
        source.text = source.text.replace(
            "pending.queue(signal, info, prepared);",
            "publish_later(signal, info);",
        );
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("pending must publish together"))
        );
    }
    #[test]
    fn rejects_realtime_records_without_actual_recipient_charge() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == "kernel/src/task/model/signal_pending.rs")
            .unwrap();
        source.text = source
            .text
            .replace("_charge: crate::namespace::RlimitCharge", "_charge: usize");
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("actual owned FIFO record"))
        );
    }
    #[test]
    fn rejects_consumed_record_drop_under_pending_owner() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == "kernel/src/task/model/signal_dequeue.rs")
            .unwrap();
        assert!(source.text.contains("drop(pending);"));
        source.text = source.text.replace("drop(pending);", "");
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("both pending owner guards"))
        );
    }
    #[test]
    fn rejects_observer_delayed_thread_signal_retirement() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == EXIT)
            .unwrap();
        source.text = source.text.replace("task.retire_thread_signals();", "");
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("independently of observer pins"))
        );
    }
    #[test]
    fn rejects_timer_expiry_that_rearms_before_notification_is_consumed() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| {
                source.relative == "kernel/src/task/process_table/timer_queue/posix_schedule.rs"
            })
            .unwrap();
        source.text = source.text.replace(
            "phase: NotificationPhase::Publication,",
            "phase: next_period(),",
        );
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("create owns one precharged"))
        );
    }
    #[test]
    fn rejects_timer_delivery_without_actual_identity_validation() {
        let mut sources = fixture();
        let source = sources
            .iter_mut()
            .find(|source| {
                source.relative == "kernel/src/task/process_table/timer_queue/posix_schedule.rs"
            })
            .unwrap();
        source.text = source.text.replace(
            "Arc::ptr_eq(&timer.signal, &event.owner)",
            "numeric_id_matches()",
        );
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("create owns one precharged"))
        );
    }
}
