//! actual futex source transaction、不可失败 cleanup 与 RMW-before-compare 围栏。
use super::SourceFile;
use quote::ToTokens;
const FUTEX: &str = "kernel/src/task/process_table/futex.rs";
fn normalized(source: &str) -> String {
    source.split_whitespace().collect()
}
fn require(sources: &[SourceFile], path: &str, clauses: &[&str], errors: &mut Vec<String>) {
    let Some(source) = sources.iter().find(|source| source.relative == path) else {
        errors.push(format!("futex-owner: missing {path}"));
        return;
    };
    let text = normalized(&source.text);
    for clause in clauses {
        if !text.contains(&normalized(clause)) {
            errors.push(format!("futex-owner: {path}: missing {clause}"));
        }
    }
}
fn ordered(source: &SourceFile, function: &str, clauses: &[&str], errors: &mut Vec<String>) {
    let Some(body) = source.syntax.items.iter().find_map(|item| match item {
        syn::Item::Fn(item) if item.sig.ident == function => {
            Some(normalized(&item.block.to_token_stream().to_string()))
        }
        _ => None,
    }) else {
        errors.push(format!("futex-owner: missing {function}"));
        return;
    };
    let mut end = 0;
    for clause in clauses {
        let clause = normalized(clause);
        let Some(position) = body[end..].find(&clause) else {
            errors.push(format!(
                "futex-owner: {function}: lost ordered stage {clause}"
            ));
            return;
        };
        end += position + clause.len();
    }
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    require(
        sources,
        "kernel/src/task/model.rs",
        &[
            "syscall_restart: Mutex<SyscallRestartState>",
            "restart.arm(SyscallRestart",
            "set_syscall_restart_block",
            "take_syscall_restart_block",
        ],
        errors,
    );
    require(
        sources,
        "kernel/src/task/model/syscall_restart.rs",
        &[
            "block: Option<SyscallRestartBlock>",
            "self.block.take()",
            "SyscallRestartPolicy::RestartBlock",
            "syscall_abi::SYSCALL_RESTART_SYSCALL",
        ],
        errors,
    );
    require(
        sources,
        "kernel/src/syscall/futex.rs",
        &[
            "wait_with_deadline(task",
            "SyscallRestartBlock::Futex",
            "address, expected, private, deadline, bitset",
            "INTERNAL_RESTART_BLOCK",
        ],
        errors,
    );
    require(
        sources,
        "kernel/src/syscall/restart.rs",
        &[
            "task.take_syscall_restart_block()",
            "Some(deadline)",
            "sleep_with_deadline(deadline, remaining)",
            "None => -super::errno::EINTR",
        ],
        errors,
    );
    require(
        sources,
        "kernel/src/syscall/timer.rs",
        &[
            "SyscallRestartBlock::Sleep",
            "deadline: deadline_ns",
            "WaitResult::Interrupted if !relative => super::INTERNAL_RESTART_NO_HANDLER",
        ],
        errors,
    );
    require(
        sources,
        "kernel/src/syscall/signal.rs",
        &["task.clear_syscall_restart()"],
        errors,
    );
    require(
        sources,
        "kernel/src/task/model/process_exec.rs",
        &["self.clear_syscall_restart()"],
        errors,
    );
    if let Some(source) = sources
        .iter()
        .find(|source| source.relative == "kernel/src/task/model/signal_state.rs")
    {
        let text = normalized(&source.text);
        let stop = text.find("ifsignal_is_default_stop");
        let finish = stop.and_then(|start| {
            text[start..]
                .find("returnOk(SignalDelivery::Stop")
                .map(|offset| start + offset)
        });
        match (stop, finish) {
            (Some(start), Some(end)) if !text[start..end].contains("apply_syscall_restart") => {}
            _ => errors.push("futex-owner: stop must retain restart instruction until post-continue signal selection".into()),
        }
    }
    require(
        sources,
        "kernel/src/task/model/robust_list.rs",
        &[
            "RobustLink::decode",
            "pointer & !1",
            "pointer & 1 != 0",
            "memory.load_user_u32(address, fault_limits)",
            "entry.address != pending.address",
            "!entry.pi && replaced & FUTEX_WAITERS != 0",
            "self.credential_guard().lock_killable()",
            "caller.may_inspect_thread_real(self)",
            "self.thread().robust_list.lock().unwrap_or(0)",
        ],
        errors,
    );
    require(
        sources,
        "kernel/src/task/model/credentials.rs",
        &[
            "InspectionCredentials::Real => (caller.uids[0], caller.gids[0], caller.capabilities.sets.permitted,)",
            "self.may_inspect_real_identity(target.tgid(), credentials, mm)",
            "self.may_inspect_identity(credentials, mm, InspectionCredentials::Real)",
        ],
        errors,
    );
    if let Some(source) = sources
        .iter()
        .find(|source| source.relative == "kernel/src/syscall/process.rs")
    {
        ordered(
            source,
            "sys_get_robust_list",
            &[
                "crate::task::robust_list_head(pid, &task)",
                "task.copy_to_user(length_pointer, &length.to_ne_bytes())",
                "task.copy_to_user(head_pointer, &head.to_ne_bytes())",
            ],
            errors,
        );
    }
    require(
        sources,
        "kernel/src/memory/mm/user_access.rs",
        &["self.futex_atomic_word(address, limits)?.compare_exchange(current, new)"],
        errors,
    );
    require(
        sources,
        "kernel/src/task/process_table/wait_registry.rs",
        &["futex_operations: [crate::sync::TaskMutex<()>; WAIT_SHARD_COUNT]"],
        errors,
    );
    require(
        sources,
        "kernel/src/task/process_table/wait_registry/futex_transaction.rs",
        &[
            "needed[WaitIndexKey::Futex { key, id: 0 }.shard()] = true",
            "needed.into_iter().enumerate()",
            "lock_prepared(preparation)",
            "self.guards.iter_mut().rev()",
            "self.registry.wake_futex_one",
        ],
        errors,
    );
    require(
        sources,
        "kernel/src/task/process_table/wait_registry/requeue.rs",
        &[
            "self.covers(source)",
            "self.covers(target)",
            "if source == target",
            "node.set_key(target_key)",
            "commit_vacant(node)",
            "registration.state.store(previous, Ordering::Release)",
        ],
        errors,
    );
    require(
        sources,
        "kernel/src/task/process_table/wait_registry/publication.rs",
        &["REQUEUEING | CLAIMING => spin_loop()"],
        errors,
    );
    require(
        sources,
        "kernel/src/memory/mm/user_access.rs",
        &[
            "fn load_user_u32",
            "load(Ordering::Acquire)",
            "fn futex_atomic_word",
            "cast::<AtomicU32>()",
        ],
        errors,
    );
    require(
        sources,
        "kernel/src/memory/mm/futex_atomic.rs",
        &[
            "swap(value, Ordering::SeqCst)",
            "fetch_add(value, Ordering::SeqCst)",
            "fetch_or(value, Ordering::SeqCst)",
            "fetch_and(!value, Ordering::SeqCst)",
            "fetch_xor(value, Ordering::SeqCst)",
        ],
        errors,
    );
    require(
        sources,
        FUTEX,
        &[
            "Some(preparation)",
            "drop(transaction)",
            "wake_claimed(&mut claimed)",
            "[source_key, target_key] != keys",
        ],
        errors,
    );
    let Some(source) = sources.iter().find(|source| source.relative == FUTEX) else {
        return;
    };
    ordered(
        source,
        "futex_wait",
        &[
            "futex_transaction",
            "WAIT_REGISTRY.publish(wait)",
            "confirmed_key",
            "drop(transaction)",
            "published.prepare_arm()",
        ],
        errors,
    );
    ordered(
        source,
        "futex_wake_op",
        &[
            "futex_transaction",
            "futex_atomic_word",
            "word.update(update)",
            "compare(old)?",
            "transaction.wake",
            "drop(transaction)",
            "wake_claimed",
        ],
        errors,
    );
}
#[cfg(test)]
mod tests {
    use super::*;
    fn source(text: String) -> SourceFile {
        SourceFile {
            relative: FUTEX.into(),
            owner: "task".into(),
            syntax: syn::parse_file(&text).unwrap(),
            lines: text.lines().map(str::to_owned).collect(),
            text,
            binary_crate: true,
        }
    }
    #[test]
    fn removing_old_value_comparison_fails_the_atomic_operation_order_proof() {
        let actual = include_str!("../../../kernel/src/task/process_table/futex.rs");
        let mut errors = Vec::new();
        ordered(
            &source(actual.into()),
            "futex_wake_op",
            &[
                "futex_transaction",
                "futex_atomic_word",
                "word.update(update)",
                "compare(old)?",
                "transaction.wake",
                "drop(transaction)",
                "wake_claimed",
            ],
            &mut errors,
        );
        assert!(errors.is_empty(), "{errors:#?}");
        let changed = actual.replace("let second = compare(old)?;", "let second = true;");
        assert_ne!(changed, actual);
        ordered(
            &source(changed),
            "futex_wake_op",
            &["word.update(update)", "compare(old)?", "transaction.wake"],
            &mut errors,
        );
        assert_eq!(errors.len(), 1, "{errors:#?}");
    }

    fn reject_registration_query(path: &str, before: &str, after: &str, missing: &str) {
        let mut sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let source = sources
            .iter_mut()
            .find(|source| source.relative == path)
            .unwrap();
        assert!(source.text.contains(before));
        source.text = source.text.replace(before, after);
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors.iter().any(|error| error.contains(missing)),
            "{errors:#?}"
        );
    }

    #[test]
    fn robust_query_rejects_authorization_without_target_exec_serialization() {
        reject_registration_query(
            "kernel/src/task/model/robust_list.rs",
            ".credential_guard()",
            ".other_guard()",
            "self.credential_guard().lock_killable()",
        );
    }

    #[test]
    fn robust_query_rejects_effective_uid_in_real_credentials_mode() {
        reject_registration_query(
            "kernel/src/task/model/credentials.rs",
            "InspectionCredentials::Real => (\n                caller.uids[0],",
            "InspectionCredentials::Real => (\n                caller.uids[1],",
            "InspectionCredentials::Real =>",
        );
    }

    #[test]
    fn robust_query_rejects_head_copyout_before_length() {
        reject_registration_query(
            "kernel/src/syscall/process.rs",
            ".copy_to_user(length_pointer, &length.to_ne_bytes())",
            ".copy_to_user(head_pointer, &head.to_ne_bytes())",
            "sys_get_robust_list: lost ordered stage",
        );
    }
}
