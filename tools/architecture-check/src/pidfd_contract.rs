//! PIDFD 的实际 association、native file operations 与 copyout/publication 顺序围栏。
use super::SourceFile;
use quote::ToTokens;

const PIDFD: &str = "kernel/src/execution/task/process_table/pidfd.rs";
const EXIT: &str = "kernel/src/execution/task/process_table/process_exit.rs";
const PROCESS: &str = "kernel/src/syscall/process.rs";
fn normalized(source: &str) -> String {
    source.split_whitespace().collect()
}
fn require(sources: &[SourceFile], path: &str, clauses: &[&str], errors: &mut Vec<String>) {
    let Some(source) = sources.iter().find(|source| source.relative == path) else {
        errors.push(format!("pidfd-owner: missing {path}"));
        return;
    };
    let text = normalized(&source.syntax.to_token_stream().to_string());
    for clause in clauses {
        if !text.contains(&normalized(clause)) {
            errors.push(format!("pidfd-owner: {path}: missing {clause}"));
        }
    }
}
fn body(source: &SourceFile, function: &str) -> Option<String> {
    source.syntax.items.iter().find_map(|item| match item {
        syn::Item::Fn(item) if item.sig.ident == function => {
            Some(normalized(&item.block.to_token_stream().to_string()))
        }
        syn::Item::Impl(item) => item.items.iter().find_map(|item| match item {
            syn::ImplItem::Fn(item) if item.sig.ident == function => {
                Some(normalized(&item.block.to_token_stream().to_string()))
            }
            _ => None,
        }),
        _ => None,
    })
}
fn ordered(source: &SourceFile, function: &str, clauses: &[&str], errors: &mut Vec<String>) {
    let Some(body) = body(source, function) else {
        errors.push(format!("pidfd-owner: missing {function}"));
        return;
    };
    let mut cursor = 0;
    for clause in clauses {
        let clause = normalized(clause);
        let Some(offset) = body[cursor..].find(&clause) else {
            errors.push(format!(
                "pidfd-owner: {function}: lost ordered stage {clause}"
            ));
            return;
        };
        cursor += offset + clause.len();
    }
}
pub(super) fn check(sources: &[SourceFile], errors: &mut Vec<String>) {
    require(
        sources,
        "kernel/src/fs/pidfs.rs",
        &[
            "exit_status: spin::Mutex<Option<i32>>",
            "static INODES: LiveInodes<PidInode, usize>",
            "inode.target.source().clone()",
            "fn supports_read(&self) -> bool { false }",
            "fn supports_write(&self) -> bool { false }",
            "fn pid_reference(&self)",
            "PidFdPoll::Reaped",
            "numbers_in(view)",
            "ioctl::ioctl(self.0.as_ref(), call)",
        ],
        errors,
    );
    require(
        sources,
        "kernel/src/fs/pidfs/ioctl.rs",
        &[
            "target.namespace(selector, call.argument != 0)?",
            "target.source().exit_status()",
            "target.information(&view)",
            "errno::EREMOTE",
            "size < 88",
            "provided &= !32",
        ],
        errors,
    );
    require(
        sources,
        "kernel/src/execution/task/model/file_descriptions.rs",
        &["self.fd_get_raw(fd).filter(|file| !file.is_path())"],
        errors,
    );
    require(
        sources,
        "kernel/src/file/description_preparation.rs",
        &["self.initialize(OpenFileKind::Inode, opened, flags, None, None)"],
        errors,
    );
    require(
        sources,
        "kernel/src/execution/task/model/file_table.rs",
        &[
            "self.credential_guard().lock_killable()",
            "caller.may_inspect_thread_real(self)",
            "crate::execution::task::process_table::pidfd::file_accessible(self)",
            "owner.with_table(|table| table.get_with_flags(fd))",
        ],
        errors,
    );
    require(
        sources,
        PROCESS,
        &[
            "ChildWaitSelector::Pid(identity)",
            "nonblocking || options & NOHANG != 0",
            "options & NOWAIT != 0",
            "crate::execution::task::release_child_status(record)",
            "task.validate_user_write(information, 128)",
        ],
        errors,
    );
    require(
        sources,
        "kernel/src/execution/task/process_table/signal.rs",
        &[
            "sender.may_signal_thread(thread)",
            "super::thread_selector::execution_by_tid(&graph, pid)",
        ],
        errors,
    );
    require(
        sources,
        "kernel/src/execution/task/process_table/signal/pidfd.rs",
        &["ProcessSelector::ThreadGroupOfPid(identity.key())"],
        errors,
    );
    if let Some(source) = sources.iter().find(|source| source.relative == PIDFD) {
        let target = source.syntax.items.iter().find_map(|item| match item {
            syn::Item::Struct(item) if item.ident == "PidTarget" => Some(item),
            _ => None,
        });
        if target.is_none_or(|target| {
            target.fields.len() != 2
                || target.fields.iter().any(|field| {
                    !field
                        .ident
                        .as_ref()
                        .is_some_and(|name| name == "identity" || name == "source")
                })
        }) {
            errors.push(
                "pidfd-owner: PID observer must not pin Task, MM, files or duplicate live phase"
                    .into(),
            );
        }
        ordered(
            source,
            "for_clone",
            &["Self::prepare", "caller.copy_to_user", "Ok(prepared)"],
            errors,
        );
    }
    if let Some(source) = sources.iter().find(|source| source.relative == EXIT) {
        ordered(
            source,
            "prepare_current_exit",
            &[
                "exit_memory.complete()",
                "completion.complete()",
                ".take_entry(&task.tid())",
                "drop(retired_thread_index)",
                "thread_retirement.complete()",
                "complete_process_exit(task.tgid())",
            ],
            errors,
        );
    }
    if let Some(source) = sources.iter().find(|source| source.relative == PROCESS) {
        ordered(
            source,
            "sys_wait4",
            &[
                "child_status_pid(&record)",
                "consume_child_status(record)",
                ".copy_to_user(status as usize",
            ],
            errors,
        );
        ordered(
            source,
            "sys_waitid",
            &[
                "consume_child_status(record)",
                "task.validate_user_write(information, 128)",
            ],
            errors,
        );
    }
    if let Some(source) = sources
        .iter()
        .find(|source| source.relative == "kernel/src/file/mod.rs")
    {
        for item in &source.syntax.items {
            if let syn::Item::Enum(item) = item
                && item.ident == "OpenFileKind"
                && item.variants.iter().any(|variant| variant.ident == "PidFd")
            {
                errors
                    .push("pidfd-owner: PIDFD must use actual pidfs device file operations".into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reject(path: &str, before: &str, after: &str, diagnostic: &str) {
        let mut sources = super::super::load_sources(&super::super::repository_root()).unwrap();
        let mut original = Vec::new();
        check(&sources, &mut original);
        assert!(original.is_empty(), "{original:#?}");
        let source = sources
            .iter_mut()
            .find(|source| source.relative == path)
            .unwrap();
        assert!(source.text.contains(before));
        source.text = source.text.replacen(before, after, 1);
        source.syntax = syn::parse_file(&source.text).unwrap();
        let mut errors = Vec::new();
        check(&sources, &mut errors);
        assert!(
            errors.iter().any(|error| error.contains(diagnostic)),
            "{errors:#?}"
        );
    }
    #[test]
    fn task_pin_in_pid_observer_fails_the_owner_proof() {
        reject(
            PIDFD,
            "struct PidTarget {",
            "struct PidTarget { task: Arc<TaskControlBlock>,",
            "must not pin Task",
        );
    }
    #[test]
    fn path_descriptor_cannot_acquire_device_operations() {
        reject(
            "kernel/src/execution/task/model/file_descriptions.rs",
            ".filter(|file| !file.is_path())",
            ".filter(|_| true)",
            "fd_get_raw(fd)",
        );
    }
    #[test]
    fn publishing_process_exit_before_real_cleanup_is_rejected() {
        reject(
            EXIT,
            "exit_memory.complete();",
            "complete_process_exit(task.tgid());",
            "prepare_current_exit: lost ordered stage",
        );
    }
    #[test]
    fn copy_fault_cannot_restore_consumed_wait_event() {
        reject(
            PROCESS,
            "consume_child_status(record);",
            "crate::execution::task::release_child_status(record);",
            "sys_wait4: lost ordered stage",
        );
    }
    #[test]
    fn clone_pidfd_cannot_publish_without_strict_fd_copyout() {
        reject(
            PIDFD,
            ".copy_to_user(",
            ".best_effort_copy_to_user(",
            "for_clone: lost ordered stage",
        );
    }
}
