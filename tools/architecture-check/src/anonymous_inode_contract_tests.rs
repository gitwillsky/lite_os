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
fn accepts_single_identity_and_transaction_owners() {
    let mut errors = Vec::new();
    check(&fixture(), &mut errors);
    assert!(errors.is_empty(), "{errors:?}");
}
#[test]
fn rejects_label_proc_target() {
    reject(
        "kernel/src/fs/procfs/snapshot.rs",
        "target: Arc<super::super::OpenedFile>",
        "target: Vec<u8>",
        "without label fallback",
    );
}
#[test]
fn rejects_pipe_endpoint_pin_in_inode() {
    reject(
        "kernel/src/fs/anonymous.rs",
        "Mutex<Weak<Pipe>>",
        "Mutex<Arc<Pipe>>",
        "must not pin endpoints",
    );
}
#[test]
fn rejects_retired_session_readmission() {
    reject(
        "kernel/src/ipc/pipe/pipe_open.rs",
        "state.session == PipeSession::Retired",
        "state.session == PipeSession::Fresh",
        "one state transaction",
    );
}
#[test]
fn rejects_named_fifo_hup_suppression_with_existing_writer() {
    reject(
        "kernel/src/fs/fifo.rs",
        "&& prior.writers == 0",
        "&& prior.writers != 0",
        "HUP suppression",
    );
}
#[test]
fn rejects_seal_race_outside_write_sequence() {
    reject(
        "kernel/src/fs/memory_file.rs",
        "let _sequence = self\n            .write_sequence\n            .lock()\n            .map_err(|_| FileSystemError::OutOfMemory)?;\n        let mut state = self.state()?",
        "let _sequence = ();\n        let mut state = self.state()?",
        "seal rejection/data failure",
    );
}
#[test]
fn rejects_metadata_commit_before_resize_failure() {
    reject(
        "kernel/src/fs/memory_file.rs",
        "let mut metadata = prepare()?;",
        "let mut metadata = prepare()?; commit(&mut metadata);",
        "data failure",
    );
}
#[test]
fn rejects_future_mtime_becoming_ctime() {
    reject(
        "kernel/src/fs/inode_times.rs",
        "times.ctime = now;",
        "times.ctime = times.mtime;",
        "explicit mtime distinct",
    );
}
#[test]
fn rejects_private_anon_fsnotify() {
    reject(
        "kernel/src/fs/inotify.rs",
        "!inode.allows_notifications()",
        "false",
        "non-private inode",
    );
}

#[test]
fn rejects_write_seal_publication_before_mapping_admission_check() {
    reject(
        "kernel/src/fs/memory_file.rs",
        "candidate.add(seals).map_err(seal_error)?;",
        "candidate.add(seals).map_err(seal_error)?; state.seals = candidate;",
        "seal rejection/data failure",
    );
}
