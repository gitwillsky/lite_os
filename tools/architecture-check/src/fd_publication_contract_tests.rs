use super::*;
fn fixture() -> Vec<SourceFile> {
    super::super::load_sources(&super::super::repository_root()).unwrap()
}
fn reject(path: &str, old: &str, new: &str, expected: &str) {
    let mut sources = fixture();
    let source = sources.iter_mut().find(|s| s.relative == path).unwrap();
    assert!(source.text.contains(old));
    source.text = source.text.replace(old, new);
    source.syntax = syn::parse_file(&source.text).unwrap();
    let mut errors = Vec::new();
    check(&sources, &mut errors);
    assert!(errors.iter().any(|e| e.contains(expected)), "{errors:?}");
}
#[test]
fn accepts_single_fd_admission_and_publication_owner() {
    let mut errors = Vec::new();
    check(&fixture(), &mut errors);
    assert!(errors.is_empty(), "{errors:?}");
}
#[test]
fn rejects_first_side_effect_before_reservation_even_when_normal_open_remains() {
    reject(
        "kernel/src/syscall/fs/open.rs",
        "let reservation = match task.fd_prepare_open",
        "let _early = base(task, fd, &path);\n    let reservation = match task.fd_prepare_open",
        "before every",
    );
}
#[test]
fn rejects_rollback_through_replacement_thread_handle() {
    reject(
        "kernel/src/task/model/file_table.rs",
        "self.owner.with_table(|files| files.cancel_reserved(fd))",
        "self.task.with_files(|files| files.cancel_reserved(fd))",
        "original FileTable",
    );
}
#[test]
fn rejects_allocating_publication() {
    reject(
        "kernel/src/file/descriptor_table.rs",
        "entry.ofd = Some(file);",
        "let _allocation = Vec::<u8>::new().try_reserve(8);\n        entry.ofd = Some(file);",
        "allocation-free",
    );
}
#[test]
fn rejects_copying_unpublished_open_to_fork() {
    reject(
        "kernel/src/file/descriptor_table.rs",
        "try_clone_where(|entry| entry.published)",
        "try_clone_where(|_| true)",
        "invisible and exclusive",
    );
}
#[test]
fn rejects_exposing_incomplete_descriptor() {
    reject(
        "kernel/src/file/descriptor_table.rs",
        "entry.published = true;\n    }\n\n    /// 无分配摘除 copyout",
        "entry.published = false;\n    }\n\n    /// 无分配摘除 copyout",
        "allocation-free",
    );
}

#[test]
fn rejects_oom_drop_of_unopened_description() {
    reject(
        "kernel/src/file/description_preparation.rs",
        "allocation: Arc<MaybeUninit<OpenFileDescription>>",
        "allocation: Arc<OpenFileDescription>",
        "initialized before",
    );
}
#[test]
fn rejects_truncate_before_open_notification() {
    reject(
        "kernel/src/syscall/fs/open.rs",
        "ofd.notify(crate::fs::IN_OPEN);",
        "let _early = crate::fs::truncate(inode.clone(), 0, &identity);\n    ofd.notify(crate::fs::IN_OPEN);",
        "initialized before",
    );
}

#[test]
fn rejects_regular_device_backend_bypassing_mount_writer() {
    reject(
        "kernel/src/file/description_preparation.rs",
        "InodeType::File | InodeType::BlockDevice",
        "InodeType::BlockDevice",
        "pre-open writer admission",
    );
}
#[test]
fn rejects_losing_prepared_writer_at_backend_commit() {
    reject(
        "kernel/src/file/description_preparation.rs",
        "self.write_open",
        "None",
        "pre-open writer admission",
    );
}

#[test]
fn rejects_accept_after_consuming_connection_fd_admission() {
    reject(
        "kernel/src/syscall/socket.rs",
        "let reservation = match task.fd_prepare_open(flags & SOCK_CLOEXEC != 0)",
        "let _early = prepared.accept(&listener, None);\n    let reservation = match task.fd_prepare_open(flags & SOCK_CLOEXEC != 0)",
        "before consuming connection",
    );
}
#[test]
fn rejects_accept_fd_published_before_user_address_copy() {
    reject(
        "kernel/src/syscall/socket.rs",
        "if let Err(error) = socket",
        "let _early = reservation.publish(accepted);\n                if let Err(error) = socket",
        "before consuming connection",
    );
}
#[test]
fn rejects_paired_fd_transient_visibility() {
    reject(
        "kernel/src/syscall/socket.rs",
        "if task.copy_to_user(output, &bytes).is_err()",
        "reservation.publish_many([first_ofd, second_ofd]);\n    if task.copy_to_user(output, &bytes).is_err()",
        "atomic paired publication",
    );
}

#[test]
fn rejects_scm_early_publication_even_if_late_publication_remains() {
    reject(
        "kernel/src/syscall/socket/control.rs",
        "let descriptor = reservation.descriptors()[0];",
        "reservation.publish(file.clone());\n        let descriptor = reservation.descriptors()[0];",
        "native SCM",
    );
}
#[test]
fn rejects_scm_header_before_fd_installation() {
    reject(
        "kernel/src/syscall/socket/control.rs",
        "installed += 1;",
        "let header = ();\n        installed += 1;",
        "native SCM",
    );
}
#[test]
fn rejects_scm_control_copy_fault_becoming_recvmsg_efault() {
    reject(
        "kernel/src/syscall/socket/control.rs",
        "Err(_) => break,",
        "Err(_) => return Err(-errno::EFAULT),",
        "native SCM",
    );
}
#[test]
fn rejects_scm_controllen_before_flags() {
    reject(
        "kernel/src/syscall/socket/control.rs",
        "if name != 0 {",
        "task.copy_to_user(message.wrapping_add(40), &written.to_ne_bytes());\n    if name != 0 {",
        "native SCM",
    );
}
