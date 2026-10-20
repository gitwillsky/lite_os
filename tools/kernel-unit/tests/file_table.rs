#![feature(allocator_ext)]
#![allow(dead_code)]
//! Actual FileTable/IndexedSlots/POSIX owners. Device, OFD payload and scheduler are host seams.
extern crate alloc;
#[path = "../src/ipc_namespace_fixture.rs"]
mod ipc;

#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/mount_membership_fixture.rs"]
mod mount_membership_fixture;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../src/socket_core_fixture.rs"]
mod socket;
use alloc::sync::Arc;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::{Cell, RefCell},
};
thread_local! { static FAIL_NEXT: Cell<bool> = const { Cell::new(false) }; }
thread_local! { static FAIL_AFTER: Cell<Option<usize>> = const { Cell::new(None) }; }
struct FaultAllocator;
// SAFETY: only injected allocation fails; successful allocations and deallocations delegate to System.
unsafe impl GlobalAlloc for FaultAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let injected = FAIL_AFTER
            .try_with(|value| match value.get() {
                Some(0) => {
                    value.set(None);
                    true
                }
                Some(remaining) => {
                    value.set(Some(remaining - 1));
                    false
                }
                None => false,
            })
            .unwrap_or(false);
        if injected
            || FAIL_NEXT
                .try_with(|flag| flag.replace(false))
                .unwrap_or(false)
        {
            assert!(task::current_task().is_none_or(|task| task.handle_unlocked()));
            return core::ptr::null_mut();
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }
}
#[global_allocator]
static ALLOCATOR: FaultAllocator = FaultAllocator;

#[path = "../../../kernel/src/sync"]
mod sync {
    mod wait_completion;
    pub(crate) use wait_completion::WaitCompletion;
    mod task_wait;
    pub(crate) use task_wait::{
        TaskWaitKey, TaskWaitTarget, WaitResult, install_wait_target_factory,
    };
    mod task_event;
    pub(crate) use task_event::TaskEvent;
    #[allow(
        unused_imports,
        reason = "focused fixture does not consume every TaskMutex lock mode"
    )]
    mod task_mutex;
    pub(crate) use task_mutex::{
        TaskMutex, TaskMutexGuard, TaskMutexLockError, TaskMutexWaitPreparation,
    };
}
#[path = "../src/interruptible_event_target.rs"]
mod scheduler_target;
#[path = "../../../kernel/src/memory"]
mod memory {
    #[path = "../../../tools/kernel-unit/src/shm_backing_fixture.rs"]
    mod backing_input;
    pub(crate) use backing_input::{PAGE_SIZE, SharedMemoryBacking};
    #[derive(Debug)]
    pub(crate) enum MemoryError {
        OutOfMemory,
    }
}
mod tty {
    pub(crate) struct Terminal;
    impl Terminal {
        pub(crate) fn proc_identity(&self, _: &crate::namespace::PidNamespace) -> (u32, isize) {
            (0, -1)
        }
    }
}

#[path = "../../../kernel/src/fs"]
mod fs {
    pub(crate) use crate::mount_membership_fixture::MountNamespace;
    use alloc::{sync::Arc, vec::Vec};
    use core::sync::atomic::{AtomicUsize, Ordering};
    use spin::Mutex;
    pub(crate) const O_RDONLY: u32 = 0;
    pub(crate) const O_WRONLY: u32 = 1;
    pub(crate) struct AccessIdentity;
    impl AccessIdentity {
        pub(crate) fn root() -> Self {
            Self
        }
    }
    #[derive(Debug)]
    pub(crate) enum FileSystemError {
        OutOfMemory,
    }
    pub(crate) enum InodeOpen {
        Device(usize),
        Storage,
    }
    pub(crate) struct OpenedFile {
        pub(crate) identity: usize,
    }

    #[path = "vfs/lookup_origin.rs"]
    mod lookup_origin;
    pub(crate) use lookup_origin::LookupOrigin;
    pub(crate) enum ProcDirectory {
        Root,
        Cwd,
    }
    pub(crate) struct Inode(usize);
    impl OpenedFile {
        pub(crate) fn inode(&self) -> Inode {
            Inode(self.identity)
        }
    }
    impl Inode {
        pub(crate) fn device_number(&self) -> Option<usize> {
            Some(self.0)
        }
    }
    pub(crate) struct ProcFileDescriptorSnapshot {
        pub(crate) fd: usize,
        pub(crate) target: Arc<OpenedFile>,
    }
    impl VirtualFileSystem {
        pub(crate) fn initial_namespace(&self) -> Arc<MountNamespace> {
            crate::mount_membership_fixture::initial()
        }
        pub(crate) fn namespace_root(
            &self,
            _: &Arc<MountNamespace>,
        ) -> Result<Arc<OpenedFile>, FileSystemError> {
            self.open_file(b"/")
        }
        pub(crate) fn is_namespace_root(
            &self,
            _: &Arc<MountNamespace>,
            _: &Arc<OpenedFile>,
        ) -> bool {
            true
        }
    }
    pub(crate) mod chardev {
        pub(crate) enum DeviceError {
            Errno(isize),
        }
        pub(crate) struct OpenRequest<'a> {
            pub(crate) number: usize,
            pub(crate) flags: u32,
            pub(crate) identity: &'a super::AccessIdentity,
        }
        pub(crate) fn open(request: &OpenRequest<'_>) -> Result<usize, super::FileSystemError> {
            Ok(request.number)
        }
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub(crate) struct AdvisoryLockKey(pub(crate) usize);
    #[derive(Debug)]
    pub(crate) enum AdvisoryLockError {
        NoLocks,
    }
    #[derive(Debug)]
    pub(crate) enum AdvisoryLockAttempt {
        Acquired {
            key: AdvisoryLockKey,
            wake_waiters: bool,
        },
        Blocked {
            key: AdvisoryLockKey,
            wake_waiters: bool,
        },
    }
    pub(crate) mod advisory_lock {
        pub(crate) struct LockHolder {
            pub(crate) identity: usize,
            pub(crate) inode: usize,
        }
        pub(crate) enum PreparedLockAttempt {
            Complete(super::AdvisoryLockAttempt),
            NeedsStorage,
        }
    }
    #[path = "vfs/record_lock.rs"]
    mod record_lock;
    pub(crate) use record_lock::{RecordLockMode, RecordLockOwner, RecordLockRange};
    pub(crate) struct VirtualFileSystem {
        record_locks: Mutex<Vec<record_lock::RecordLock>>,
        notifications: AtomicUsize,
    }
    impl VirtualFileSystem {
        fn advisory_identity(
            holder: &advisory_lock::LockHolder,
        ) -> Result<(AdvisoryLockKey, ()), AdvisoryLockError> {
            Ok((AdvisoryLockKey(holder.inode), ()))
        }
        fn notify_advisory_lock(&self, _: AdvisoryLockKey) {
            self.notifications.fetch_add(1, Ordering::Relaxed);
        }
        pub(crate) fn release_advisory_lock(&self, _: usize) {}
        pub(crate) fn open_file(&self, _: &[u8]) -> Result<Arc<OpenedFile>, FileSystemError> {
            static NEXT: AtomicUsize = AtomicUsize::new(1);
            Ok(Arc::new(OpenedFile {
                identity: NEXT.fetch_add(1, Ordering::Relaxed),
            }))
        }
    }
    pub(crate) fn vfs() -> &'static VirtualFileSystem {
        static VFS: std::sync::OnceLock<VirtualFileSystem> = std::sync::OnceLock::new();
        VFS.get_or_init(|| VirtualFileSystem {
            record_locks: Mutex::new(Vec::new()),
            notifications: AtomicUsize::new(0),
        })
    }
}

#[path = "../../../kernel/src/file"]
mod file {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};
    pub(crate) struct OpenFileDescription {
        pub(crate) descriptor_refs: AtomicUsize,
        opened: Arc<crate::fs::OpenedFile>,
        pub(crate) epoll_retirements: AtomicUsize,
        flags: u32,
    }
    impl OpenFileDescription {
        pub(crate) fn is_path(&self) -> bool {
            self.flags & 0x200000 != 0
        }
        pub(crate) fn file_information(
            &self,
            _: u32,
            _: &crate::namespace::PidNamespace,
        ) -> Result<alloc::vec::Vec<u8>, crate::fs::FileSystemError> {
            unreachable!("fdinfo codec is outside file-table ownership fixture")
        }
        pub(crate) fn device(
            _: usize,
            flags: u32,
            opened: Arc<crate::fs::OpenedFile>,
        ) -> Result<Arc<Self>, crate::fs::FileSystemError> {
            Ok(Arc::new(Self {
                descriptor_refs: AtomicUsize::new(0),
                opened,
                epoll_retirements: AtomicUsize::new(0),
                flags,
            }))
        }
        pub(crate) fn lock_holder(&self) -> crate::fs::advisory_lock::LockHolder {
            crate::fs::advisory_lock::LockHolder {
                identity: core::ptr::from_ref(self).addr(),
                inode: self.opened.identity,
            }
        }
        pub(crate) fn opened_ref(&self) -> Arc<crate::fs::OpenedFile> {
            self.opened.clone()
        }
    }
    pub(crate) struct PreparedFileDescription;
    impl PreparedFileDescription {
        pub(crate) fn try_new() -> Result<Self, crate::fs::FileSystemError> {
            Ok(Self)
        }
        pub(crate) fn prepare_named(
            self,
            opened: Arc<crate::fs::OpenedFile>,
            flags: u32,
        ) -> Result<PreparedNamedFile, crate::fs::FileSystemError> {
            Ok(PreparedNamedFile { opened, flags })
        }
    }
    pub(crate) struct PreparedNamedFile {
        opened: Arc<crate::fs::OpenedFile>,
        flags: u32,
    }
    impl PreparedNamedFile {
        pub(crate) fn finish(self, backend: crate::fs::InodeOpen) -> Arc<OpenFileDescription> {
            let crate::fs::InodeOpen::Device(device) = backend else {
                unreachable!("console backend seam")
            };
            OpenFileDescription::device(device, self.flags, self.opened).unwrap()
        }
    }
    pub(crate) struct Epoll;
    impl Epoll {
        pub(crate) fn release_file(file: &Arc<OpenFileDescription>) {
            file.epoll_retirements.fetch_add(1, Ordering::Relaxed);
        }
    }
    mod descriptor_table;
    pub(crate) use descriptor_table::{
        DetachedFileDescriptor, FileDescriptorError, FileDescriptorTable,
    };
}

#[path = "../../../kernel/src/task"]
mod task {
    use super::{Arc, RefCell};
    thread_local! { static CURRENT: RefCell<Option<Arc<TaskControlBlock>>> = const { RefCell::new(None) }; }
    pub(crate) use model::TaskControlBlock;
    pub(crate) fn current_task() -> Option<Arc<TaskControlBlock>> {
        CURRENT.with(|value| value.borrow().clone())
    }
    pub(crate) fn enter(task: &Arc<TaskControlBlock>) {
        CURRENT.with(|value| *value.borrow_mut() = Some(task.clone()));
    }
    pub(crate) enum PidFdError {
        NotFound,
        Permission,
        NoMemory,
        Interrupted,
        BadDescriptor,
    }
    pub(crate) mod process_table {
        pub(crate) mod pidfd {
            pub(crate) fn file_accessible(_: &super::super::TaskControlBlock) -> bool {
                unreachable!("PIDFD graph eligibility belongs to PIDFD fixtures")
            }
        }
        pub(crate) fn is_single_threaded(_: &super::TaskControlBlock) -> bool {
            true
        }
    }
    pub(crate) mod model {
        use alloc::sync::Arc;
        use spin::Mutex;
        mod file_table;
        use file_table::{FileTableHandle, FileTableOwner, try_file_handle};
        mod file_descriptions;
        mod namespaces;
        mod process_resources;
        use namespaces::FsNamespaceHandle;
        use process_resources::FsContext;
        struct ProcessId(usize);
        pub(crate) struct Process {
            tgid: ProcessId,
            leader_files: Mutex<FileTableHandle>,
            terminal: Mutex<Option<Arc<crate::tty::Terminal>>>,
        }
        struct Thread {
            files: FileTableHandle,
            fs_namespaces: FsNamespaceHandle,
            file_wait: Mutex<Option<crate::sync::TaskMutexWaitPreparation>>,
        }
        struct Execution {
            tid: core::sync::atomic::AtomicUsize,
        }
        // execution variant是显式constructor边界；namespace tuple/retirement全部使用production。
        struct KernelNamespaces {
            namespaces: Mutex<Option<namespaces::NamespaceMembership>>,
        }
        enum TaskKind {
            User {},
            Kernel(KernelNamespaces),
        }
        pub(crate) struct TaskControlBlock {
            kind: TaskKind,
            process: Arc<Process>,
            thread: Thread,
            execution: Execution,
        }
        impl Process {
            pub(crate) fn snapshot_descriptors(
                &self,
            ) -> Result<
                alloc::vec::Vec<crate::fs::ProcFileDescriptorSnapshot>,
                crate::fs::FileSystemError,
            > {
                self.proc_file_descriptors()
            }
        }
        impl TaskControlBlock {
            pub(crate) fn snapshot_descriptors(
                &self,
            ) -> Result<
                alloc::vec::Vec<crate::fs::ProcFileDescriptorSnapshot>,
                crate::fs::FileSystemError,
            > {
                self.proc_file_descriptors()
            }
            fn namespace_rlimits(&self) -> crate::namespace::NamespaceRlimits {
                unreachable!("new user namespace admission belongs to credential fixtures")
            }
            fn may_inspect_thread_real(&self, _: &Self) -> bool {
                unreachable!("REALCREDS inspection belongs to credential/PIDFD fixtures")
            }
            fn may_inspect_thread(&self, _: &Self) -> bool {
                unreachable!("FSCREDS inspection belongs to credential/PIDFD fixtures")
            }
            fn credential_guard(&self) -> &crate::sync::TaskMutex<()> {
                unreachable!("exec authorization belongs to credential/PIDFD fixtures")
            }
            pub(crate) fn tgid(&self) -> usize {
                self.process.tgid.0
            }
            pub(crate) fn tid(&self) -> usize {
                self.execution
                    .tid
                    .load(core::sync::atomic::Ordering::Acquire)
            }
            pub(crate) fn pid_namespace(&self) -> Arc<crate::namespace::PidNamespace> {
                crate::namespace::initial_pid()
            }
            pub(crate) fn process(&self) -> &Arc<Process> {
                &self.process
            }
            // 这里只装配 fs/credential namespace；SEM_UNDO 由实际 ipc owner fixture 与 guest 覆盖。
            fn replace_semaphore_undo(&self, _: crate::ipc::SemaphoreUndoHandle) {
                unreachable!("SEM_UNDO lifecycle is outside this producer seam")
            }
            fn thread(&self) -> &Thread {
                &self.thread
            }
            fn file_wait(&self) -> &Mutex<Option<crate::sync::TaskMutexWaitPreparation>> {
                &self.thread.file_wait
            }
            fn is_kernel_thread(&self) -> bool {
                matches!(self.kind, TaskKind::Kernel(_))
            }
            // Credential contents are outside the FileTable fixture's assertions.
            fn namespace_authority(&self) -> crate::namespace::NamespaceAuthority {
                crate::namespace::NamespaceAuthority {
                    namespace: crate::namespace::initial_user(),
                    effective_uid: 0,
                    effective_gid: 0,
                    effective_capabilities: syscall_abi::capability::VALID_MASK,
                }
            }
            fn user_namespace(&self) -> Arc<crate::namespace::UserNamespace> {
                crate::namespace::initial_user()
            }
            fn enter_user_namespace(&self, _: Arc<crate::namespace::UserNamespace>) {
                unreachable!("credential seam")
            }
            fn file_descriptor_limit(&self) -> usize {
                1_048_576
            }
            pub(crate) fn root(pid: usize) -> Arc<Self> {
                crate::namespace::boot();
                crate::ipc::initialize_namespaces().unwrap();
                crate::socket::boot_net();
                let table = crate::file::FileDescriptorTable::with_console().unwrap();
                let files = try_file_handle(FileTableOwner::try_new(table).unwrap()).unwrap();
                Arc::new(Self {
                    kind: TaskKind::User {},
                    process: Arc::new(Process {
                        tgid: ProcessId(pid),
                        leader_files: Mutex::new(files.clone()),
                        terminal: Mutex::new(None),
                    }),
                    thread: Thread {
                        files,
                        file_wait: Mutex::new(Some(
                            crate::sync::TaskMutexWaitPreparation::prepare().unwrap(),
                        )),
                        fs_namespaces: namespaces::initial_fs_namespace_handle(Arc::new(
                            Mutex::new(FsContext {
                                cwd: crate::fs::vfs().open_file(b"/").unwrap(),
                                root: crate::fs::vfs().open_file(b"/").unwrap(),
                                umask: 0o022,
                            }),
                        ))
                        .unwrap(),
                    },
                    execution: Execution {
                        tid: core::sync::atomic::AtomicUsize::new(pid),
                    },
                })
            }
            pub(crate) fn child(&self, pid: usize, same_group: bool, share: bool) -> Arc<Self> {
                let files = self.clone_file_table(share).unwrap();
                let process = if same_group {
                    self.process.clone()
                } else {
                    Arc::new(Process {
                        tgid: ProcessId(pid),
                        leader_files: Mutex::new(files.clone()),
                        terminal: Mutex::new(self.process.terminal.lock().clone()),
                    })
                };
                Arc::new(Self {
                    kind: TaskKind::User {},
                    process,
                    thread: Thread {
                        files,
                        fs_namespaces: namespaces::try_fs_namespace_handle(
                            self.namespace_snapshot(),
                            self.clone_fs_context(true).unwrap(),
                        )
                        .unwrap(),
                        file_wait: Mutex::new(Some(
                            crate::sync::TaskMutexWaitPreparation::prepare().unwrap(),
                        )),
                    },
                    execution: Execution {
                        tid: core::sync::atomic::AtomicUsize::new(pid),
                    },
                })
            }
            pub(crate) fn exit_files(&self) {
                self.retire_file_table();
            }
            pub(crate) fn exec_files(&self) {
                let prepared = self.prepare_file_table_unshare().unwrap();
                self.commit_file_table_unshare(prepared);
                let retired = core::mem::replace(
                    &mut *self.process.leader_files.lock(),
                    self.thread.files.clone(),
                );
                drop(retired);
                self.close_cloexec_files();
            }
            pub(crate) fn handle_unlocked(&self) -> bool {
                self.thread.files.try_lock().is_some() && self.thread.file_wait.try_lock().is_some()
            }
            pub(crate) fn hold_table(&self, operation: impl FnOnce()) {
                self.with_files(|_| operation());
            }
        }
    }
}

#[test]
fn sharing_unshare_and_exit_do_not_follow_tgid_or_tcb_lifetime() {
    use core::sync::atomic::Ordering;
    let parent = task::TaskControlBlock::root(1);
    task::enter(&parent);
    let shared = parent.child(2, false, true);
    let private = parent.child(3, true, false);
    let ofd = parent.fd_get(0).unwrap();
    assert_eq!(ofd.descriptor_refs.load(Ordering::Relaxed), 2);
    shared.fd_close(0).unwrap();
    assert!(parent.fd_get(0).is_none());
    assert!(private.fd_get(0).is_some());
    private.exit_files();
    assert_eq!(ofd.descriptor_refs.load(Ordering::Relaxed), 0);
    assert_eq!(ofd.epoll_retirements.load(Ordering::Relaxed), 1);
    let open = parent.fd_get(1).unwrap();
    shared.exit_files();
    parent.exit_files();
    assert_eq!(open.descriptor_refs.load(Ordering::Relaxed), 0);
    assert!(parent.process().snapshot_descriptors().unwrap().is_empty());
}

#[test]
fn file_table_identity_and_reported_pid_are_independent() {
    use fs::{RecordLockMode::Write, RecordLockRange, vfs};
    let parent = task::TaskControlBlock::root(11);
    task::enter(&parent);
    let shared = parent.child(12, false, true);
    let copied = parent.child(13, true, false);
    let ofd = parent.fd_get(0).unwrap();
    let holder = ofd.lock_holder();
    let range = RecordLockRange {
        start: 0,
        end: Some(64),
    };
    vfs()
        .try_record_lock(&holder, parent.file_lock_owner(), Some(Write), range)
        .unwrap();
    assert!(
        vfs()
            .record_lock_conflict(&holder, shared.file_lock_owner(), Write, range)
            .unwrap()
            .is_none()
    );
    let conflict = vfs()
        .record_lock_conflict(&holder, copied.file_lock_owner(), Write, range)
        .unwrap()
        .unwrap();
    assert_eq!(conflict.pid, 11);
    vfs()
        .try_record_lock(&holder, shared.file_lock_owner(), Some(Write), range)
        .unwrap();
    assert_eq!(
        vfs()
            .record_lock_conflict(&holder, copied.file_lock_owner(), Write, range)
            .unwrap()
            .unwrap()
            .pid,
        11
    );
    copied.fd_close(0).unwrap();
    assert!(
        vfs()
            .record_lock_conflict(&holder, fs::RecordLockOwner::new(1, 99), Write, range)
            .unwrap()
            .is_some()
    );
    shared.fd_close(0).unwrap();
    assert!(
        vfs()
            .record_lock_conflict(&holder, fs::RecordLockOwner::new(1, 99), Write, range)
            .unwrap()
            .is_none()
    );
}

#[test]
fn shared_unshare_oom_preserves_both_fs_and_files_and_unique_is_allocation_free() {
    let parent = task::TaskControlBlock::root(21);
    task::enter(&parent);
    let shared = parent.child(22, true, true);
    let original = parent.file_lock_owner().identity();
    FAIL_NEXT.set(true);
    assert!(
        parent
            .unshare_namespaces(
                false, true, false, false, false, false, false, false, false, false
            )
            .is_err()
    );
    assert_eq!(parent.file_lock_owner().identity(), original);
    shared.replace_umask(0o077);
    FAIL_NEXT.set(true);
    assert!(
        parent
            .unshare_namespaces(
                true, true, false, false, false, false, false, false, false, false
            )
            .is_err()
    );
    assert_eq!(parent.creation_mode(0o777), 0o700);
    parent
        .unshare_namespaces(
            true, true, false, false, false, false, false, false, false, false,
        )
        .unwrap();
    assert_ne!(parent.file_lock_owner().identity(), original);
    shared.replace_umask(0o022);
    assert_eq!(parent.creation_mode(0o777), 0o700);
    FAIL_NEXT.set(true);
    parent
        .unshare_namespaces(
            true, true, false, false, false, false, false, false, false, false,
        )
        .unwrap();
    assert!(FAIL_NEXT.replace(false));
}

#[test]
fn every_combined_unshare_allocation_failure_preserves_the_original_two_handles() {
    for stage in 0..5 {
        let parent = task::TaskControlBlock::root(31);
        task::enter(&parent);
        let shared = parent.child(32, true, true);
        let original = parent.file_lock_owner().identity();
        let source = parent.fd_get(0).unwrap();
        FAIL_AFTER.set(Some(stage));
        assert!(
            parent
                .unshare_namespaces(
                    true, true, false, false, false, false, false, false, false, false
                )
                .is_err(),
            "stage {stage}"
        );
        assert!(FAIL_AFTER.get().is_none());
        assert_eq!(parent.file_lock_owner().identity(), original);
        assert_eq!(
            source
                .descriptor_refs
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        shared.replace_umask(0o077);
        assert_eq!(parent.creation_mode(0o777), 0o700);
        shared.fd_close(0).unwrap();
        assert!(parent.fd_get(0).is_none());
    }
}

#[test]
fn exec_preserves_unique_posix_owner_and_shared_exec_cannot_release_old_owner_locks() {
    use fs::{RecordLockMode::Write, RecordLockRange, vfs};
    let parent = task::TaskControlBlock::root(41);
    task::enter(&parent);
    let ofd = parent.fd_get(0).unwrap();
    let holder = ofd.lock_holder();
    let range = RecordLockRange {
        start: 0,
        end: Some(64),
    };
    let observer = fs::RecordLockOwner::new(1, 99);
    let original = parent.file_lock_owner().identity();
    vfs()
        .try_record_lock(&holder, parent.file_lock_owner(), Some(Write), range)
        .unwrap();
    FAIL_NEXT.set(true);
    parent.exec_files();
    assert!(FAIL_NEXT.replace(false));
    assert_eq!(parent.file_lock_owner().identity(), original);
    assert!(
        vfs()
            .record_lock_conflict(&holder, observer, Write, range)
            .unwrap()
            .is_some()
    );
    let shared = parent.child(42, false, true);
    parent.fd_set_flags(1, 1).unwrap();
    parent.exec_files();
    assert_ne!(parent.file_lock_owner().identity(), original);
    assert!(parent.fd_get(1).is_none());
    assert!(shared.fd_get(1).is_some());
    assert_eq!(
        vfs()
            .record_lock_conflict(&holder, parent.file_lock_owner(), Write, range)
            .unwrap()
            .unwrap()
            .pid,
        41
    );
    shared.exit_files();
    assert!(
        vfs()
            .record_lock_conflict(&holder, parent.file_lock_owner(), Write, range)
            .unwrap()
            .is_none()
    );
    vfs()
        .try_record_lock(&holder, parent.file_lock_owner(), Some(Write), range)
        .unwrap();
    // fd0/2 是同 inode 的独立 OFD：任一 CLOEXEC close 释放该 table 在 inode 上的全部锁。
    parent.fd_set_flags(2, 1).unwrap();
    parent.exec_files();
    assert!(parent.fd_get(0).is_some());
    assert!(
        vfs()
            .record_lock_conflict(&holder, observer, Write, range)
            .unwrap()
            .is_none()
    );
}

#[test]
fn scm_single_fd_reservations_use_the_actual_open_owner_and_publish_only_after_scalar_copy() {
    use std::sync::atomic::Ordering;
    let parent = task::TaskControlBlock::root(51);
    task::enter(&parent);
    let ofd = parent.fd_get(0).unwrap();
    let received = parent.fd_prepare_open(true).unwrap();
    let fd = received.descriptors()[0];
    assert!(parent.fd_get(fd).is_none());
    let copied = parent.child(52, false, false);
    assert!(copied.fd_get(fd).is_none());
    assert_eq!(ofd.descriptor_refs.load(Ordering::Relaxed), 2);
    drop(received);
    assert_eq!(ofd.descriptor_refs.load(Ordering::Relaxed), 2);
    let received = parent.fd_prepare_open(true).unwrap();
    let fd = received.descriptors()[0];
    received.publish(ofd.clone());
    assert_eq!(ofd.descriptor_refs.load(Ordering::Relaxed), 3);
    assert_eq!(parent.fd_flags(fd).unwrap(), 1);
    parent.fd_close(fd).unwrap();
    assert_eq!(ofd.descriptor_refs.load(Ordering::Relaxed), 2);
}

#[test]
fn scm_published_prefix_survives_later_scalar_reservation_cancellation() {
    let owner = task::TaskControlBlock::root(53);
    task::enter(&owner);
    let original = owner.fd_get_raw(0).unwrap();
    let first = owner.fd_prepare_open(true).unwrap();
    let fd = first.descriptors()[0];
    first.publish(original.clone());
    let tail = owner.fd_prepare_open(true).unwrap();
    let next = tail.descriptors()[0];
    assert!(owner.fd_get_raw(next).is_none());
    drop(tail);
    assert!(Arc::ptr_eq(&owner.fd_get_raw(fd).unwrap(), &original));
    assert_eq!(owner.fd_prepare_open(true).unwrap().descriptors(), [next]);
    owner.fd_close(fd).unwrap();
}

#[test]
fn descriptor_lookup_and_contended_lookup_use_the_preallocated_waiter() {
    let parent = task::TaskControlBlock::root(61);
    task::enter(&parent);
    FAIL_NEXT.set(true);
    assert!(parent.fd_get(0).is_some());
    assert!(FAIL_NEXT.replace(false));
    let child = parent.child(62, true, true);
    let target = scheduler_target::Target::new();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (start_tx, start_rx) = std::sync::mpsc::channel();
    let join = std::thread::spawn({
        let target = target.clone();
        move || {
            task::enter(&child);
            let _binding = target.bind();
            ready_tx.send(()).unwrap();
            start_rx.recv().unwrap();
            FAIL_NEXT.set(true);
            let found = child.fd_get(0).is_some();
            let untouched = FAIL_NEXT.replace(false);
            (found, untouched)
        }
    });
    ready_rx.recv().unwrap();
    parent.hold_table(|| {
        start_tx.send(()).unwrap();
        target.wait_until_sleeping();
    });
    assert_eq!(join.join().unwrap(), (true, true));
}

#[test]
fn proc_descriptor_allocation_failure_is_distinct_from_retirement() {
    let parent = task::TaskControlBlock::root(70);
    task::enter(&parent);
    let mut failures = 0;
    for stage in 0..8 {
        FAIL_AFTER.set(Some(stage));
        let result = parent.process().snapshot_descriptors();
        FAIL_AFTER.set(None);
        match result {
            Err(fs::FileSystemError::OutOfMemory) => failures += 1,
            Ok(entries) => assert!(!entries.is_empty()),
        }
    }
    assert!(
        failures >= 2,
        "actual table snapshot and proc target snapshot allocations must both fail explicitly"
    );
    parent.exit_files();
    assert!(parent.process().snapshot_descriptors().unwrap().is_empty());
}

#[test]
fn proc_thread_descriptors_follow_own_handle_after_private_copy_and_leader_retirement() {
    let parent = task::TaskControlBlock::root(71);
    task::enter(&parent);
    let target = parent.child(72, false, false);
    target.fd_close(0).unwrap();
    assert!(
        parent
            .snapshot_descriptors()
            .unwrap()
            .iter()
            .any(|entry| entry.fd == 0)
    );
    assert!(
        target
            .snapshot_descriptors()
            .unwrap()
            .iter()
            .all(|entry| entry.fd != 0)
    );
    parent.exit_files();
    assert!(parent.snapshot_descriptors().unwrap().is_empty());
    assert!(!target.snapshot_descriptors().unwrap().is_empty());
    target.exit_files();
    assert!(target.snapshot_descriptors().unwrap().is_empty());
}

#[test]
#[ignore = "diagnostic host CPU measurement; run alone in release"]
fn file_table_lookup_snapshot_and_posix_request_cpu_cost() {
    use std::{hint::black_box, time::Instant};
    fn measure(name: &str, mut operation: impl FnMut()) {
        for _ in 0..10_000 {
            operation();
        }
        let mut samples = [0f64; 5];
        for sample in &mut samples {
            let start = Instant::now();
            for _ in 0..100_000 {
                operation();
            }
            *sample = start.elapsed().as_nanos() as f64 / 100_000.;
        }
        samples.sort_by(f64::total_cmp);
        println!("{name}: median {:.2} ns", samples[2]);
    }
    let parent = task::TaskControlBlock::root(71);
    task::enter(&parent);
    measure("FileTable fd lookup", || {
        black_box(parent.fd_get(black_box(0)));
    });
    measure("FileTable fd flags", || {
        black_box(parent.fd_flags(black_box(0)).unwrap());
    });
    let received = parent.fd_get_raw(0).unwrap();
    measure("SCM actual single-fd reserve/publish/close", || {
        let reservation = parent.fd_prepare_open(true).unwrap();
        black_box(reservation.descriptors()[0]);
        let fd = reservation.publish(received.clone());
        parent.fd_close(fd).unwrap();
    });
    let holder = parent.fd_get(0).unwrap().lock_holder();
    let owner = parent.file_lock_owner();
    let range = fs::RecordLockRange {
        start: 0,
        end: Some(64),
    };
    measure("POSIX same-owner write request", || {
        black_box(
            fs::vfs()
                .try_record_lock(&holder, owner, Some(fs::RecordLockMode::Write), range)
                .unwrap(),
        );
    });
}

#[test]
fn successful_posix_acquisition_rechecks_closed_or_reused_descriptor_identity() {
    use fs::{RecordLockMode::Write, RecordLockRange, vfs};
    for replace in [false, true] {
        let parent = task::TaskControlBlock::root(81);
        task::enter(&parent);
        let sibling = parent.child(82, true, true);
        let original = parent.fd_get(0).unwrap();
        let range = RecordLockRange {
            start: 0,
            end: Some(64),
        };
        // 模拟阻塞 waiter 已捕获旧 OFD；VFS等待/调度交错由真实musl guest执行。
        if replace {
            sibling.fd_duplicate_to(1, 0, false).unwrap();
        } else {
            sibling.fd_close(0).unwrap();
        }
        vfs()
            .try_record_lock(
                &original.lock_holder(),
                parent.file_lock_owner(),
                Some(Write),
                range,
            )
            .unwrap();
        assert!(parent.finish_record_lock(0, &original).is_err());
        let observer = fs::RecordLockOwner::new(1, 99);
        assert!(
            vfs()
                .record_lock_conflict(&original.lock_holder(), observer, Write, range)
                .unwrap()
                .is_none()
        );
        let live = parent.fd_get(2).unwrap();
        vfs()
            .try_record_lock(
                &live.lock_holder(),
                parent.file_lock_owner(),
                Some(Write),
                range,
            )
            .unwrap();
        parent.finish_record_lock(2, &live).unwrap();
        assert!(
            vfs()
                .record_lock_conflict(&live.lock_holder(), observer, Write, range)
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn open_reservation_is_invisible_exclusive_and_copied_tables_skip_it() {
    use core::sync::atomic::Ordering;
    let parent = task::TaskControlBlock::root(91);
    task::enter(&parent);
    let ofd = parent.fd_get(0).unwrap();
    let before = ofd.descriptor_refs.load(Ordering::Relaxed);
    let reservation = parent.fd_prepare_open(true).unwrap();
    assert!(parent.fd_get(3).is_none());
    assert!(parent.fd_flags(3).is_err());
    assert!(parent.fd_close(3).is_err());
    let shared = parent.child(92, false, true);
    let copied = parent.child(93, false, false);
    assert!(matches!(
        shared.fd_duplicate_to(0, 3, false),
        Err(file::FileDescriptorError::Busy)
    ));
    assert_eq!(copied.fd_duplicate(0, 0, false).unwrap(), 3);
    assert!(
        parent
            .process()
            .snapshot_descriptors()
            .unwrap()
            .iter()
            .all(|entry| entry.fd != 3)
    );
    assert_eq!(ofd.descriptor_refs.load(Ordering::Relaxed), before + 2);
    FAIL_NEXT.set(true);
    assert_eq!(reservation.publish(ofd.clone()), 3);
    assert!(FAIL_NEXT.replace(false), "publication must not allocate");
    assert_eq!(parent.fd_flags(3).unwrap(), 1);
    assert!(Arc::ptr_eq(&parent.fd_get(3).unwrap(), &ofd));
    assert!(Arc::ptr_eq(&shared.fd_get(3).unwrap(), &ofd));
}

#[test]
fn open_reservation_rollback_pins_original_owner_across_handle_unshare() {
    let parent = task::TaskControlBlock::root(94);
    task::enter(&parent);
    let shared = parent.child(95, false, true);
    let reservation = parent.fd_prepare_open(false).unwrap();
    parent.exec_files();
    // 新表未复制未发布 slot；同号新 descriptor 不得被旧 reservation Drop 删除。
    assert_eq!(parent.fd_duplicate(0, 0, false).unwrap(), 3);
    assert_eq!(shared.fd_duplicate(0, 0, false).unwrap(), 4);
    drop(reservation);
    assert!(parent.fd_get(3).is_some());
    assert_eq!(shared.fd_duplicate(0, 0, false).unwrap(), 3);
}

#[test]
fn open_reservation_backing_oom_has_no_slot_and_rollback_is_allocation_free() {
    let parent = task::TaskControlBlock::root(96);
    task::enter(&parent);
    // 0..63 在同一已物化 radix leaf；第64个 slot 必须准备下一 leaf。
    let mut reservations = Vec::new();
    for _ in 3..64 {
        reservations.push(parent.fd_prepare_open(false).unwrap());
    }
    FAIL_NEXT.set(true);
    assert!(matches!(
        parent.fd_prepare_open(false),
        Err(file::FileDescriptorError::OutOfMemory)
    ));
    assert!(!FAIL_NEXT.replace(false));
    assert!(parent.fd_get(64).is_none());
    FAIL_NEXT.set(true);
    drop(reservations);
    assert!(FAIL_NEXT.replace(false), "rollback must not allocate");
    assert_eq!(parent.fd_duplicate(0, 0, false).unwrap(), 3);
}

#[test]
fn received_actual_ofd_replaces_target_flags_and_retires_old_descriptor_outside_owner() {
    use core::sync::atomic::Ordering;
    let target = task::TaskControlBlock::root(97);
    task::enter(&target);
    let source = target.fd_get(0).unwrap();
    let old = target.fd_get(1).unwrap();
    let source_refs = source.descriptor_refs.load(Ordering::Relaxed);
    assert_eq!(target.fd_receive(source.clone(), Some(1), true), Ok(1));
    assert!(Arc::ptr_eq(&target.fd_get(1).unwrap(), &source));
    assert_eq!(target.fd_flags(1), Ok(1));
    assert_eq!(
        source.descriptor_refs.load(Ordering::Relaxed),
        source_refs + 1
    );
    assert_eq!(old.descriptor_refs.load(Ordering::Relaxed), 0);
    assert_eq!(old.epoll_retirements.load(Ordering::Relaxed), 1);
    assert_eq!(
        target.fd_receive(source, Some(usize::MAX), false),
        Err(file::FileDescriptorError::NotFound)
    );
}

#[test]
fn receive_cannot_replace_an_unpublished_reservation_and_lowest_slot_skips_it() {
    use core::sync::atomic::Ordering;
    let target = task::TaskControlBlock::root(98);
    task::enter(&target);
    let source = target.fd_get(0).unwrap();
    let before = source.descriptor_refs.load(Ordering::Relaxed);
    let reservation = target.fd_prepare_open(false).unwrap();
    assert_eq!(
        target.fd_receive(source.clone(), Some(3), true),
        Err(file::FileDescriptorError::Busy)
    );
    assert_eq!(source.descriptor_refs.load(Ordering::Relaxed), before);
    assert!(target.fd_get(3).is_none());
    assert_eq!(target.fd_receive(source.clone(), None, true), Ok(4));
    assert_eq!(target.fd_flags(4), Ok(1));
    drop(reservation);
    assert_eq!(target.fd_receive(source, None, false), Ok(3));
    assert_eq!(target.fd_flags(3), Ok(0));
}

#[test]
fn receive_backing_oom_preserves_source_and_target_descriptors() {
    use core::sync::atomic::Ordering;
    let target = task::TaskControlBlock::root(99);
    task::enter(&target);
    let source = target.fd_get(0).unwrap();
    let before = source.descriptor_refs.load(Ordering::Relaxed);
    FAIL_NEXT.set(true);
    assert_eq!(
        target.fd_receive(source.clone(), Some(64), false),
        Err(file::FileDescriptorError::OutOfMemory)
    );
    assert!(!FAIL_NEXT.replace(false));
    assert_eq!(source.descriptor_refs.load(Ordering::Relaxed), before);
    assert!(target.fd_get(64).is_none());
    assert!(Arc::ptr_eq(&target.fd_get(0).unwrap(), &source));
}

#[test]
fn paired_open_reservations_remain_invisible_and_publish_without_allocation() {
    let parent = task::TaskControlBlock::root(121);
    task::enter(&parent);
    let first = parent.fd_get(0).unwrap();
    let second = parent.fd_get(1).unwrap();
    let reservation = parent.fd_prepare_opens::<2>(true).unwrap();
    assert_eq!(reservation.descriptors(), [3, 4]);
    let shared = parent.child(122, false, true);
    assert!(shared.fd_get(3).is_none());
    assert!(shared.fd_get(4).is_none());
    assert!(matches!(
        shared.fd_duplicate_to(0, 4, false),
        Err(file::FileDescriptorError::Busy)
    ));
    FAIL_NEXT.set(true);
    assert_eq!(
        reservation.publish_many([first.clone(), second.clone()]),
        [3, 4]
    );
    assert!(
        FAIL_NEXT.replace(false),
        "paired publication must not allocate"
    );
    assert!(Arc::ptr_eq(&shared.fd_get(3).unwrap(), &first));
    assert!(Arc::ptr_eq(&shared.fd_get(4).unwrap(), &second));
    assert_eq!(shared.fd_flags(3), Ok(1));
    assert_eq!(shared.fd_flags(4), Ok(1));
}

#[test]
fn paired_open_second_slot_oom_rolls_back_first_slot_on_original_owner() {
    let parent = task::TaskControlBlock::root(123);
    task::enter(&parent);
    let mut occupied = Vec::new();
    for _ in 3..63 {
        occupied.push(parent.fd_prepare_open(false).unwrap());
    }
    FAIL_NEXT.set(true);
    assert!(matches!(
        parent.fd_prepare_opens::<2>(false),
        Err(file::FileDescriptorError::OutOfMemory)
    ));
    assert!(!FAIL_NEXT.replace(false));
    assert_eq!(parent.fd_prepare_open(false).unwrap().descriptors(), [63]);
    drop(occupied);
    assert_eq!(parent.fd_duplicate(0, 0, false), Ok(3));
}

#[test]
fn paired_open_rollback_cannot_cancel_new_owner_reusing_both_numbers() {
    let parent = task::TaskControlBlock::root(124);
    task::enter(&parent);
    let shared = parent.child(125, false, true);
    let reservation = parent.fd_prepare_opens::<2>(false).unwrap();
    parent.exec_files();
    assert_eq!(parent.fd_duplicate(0, 0, false), Ok(3));
    assert_eq!(parent.fd_duplicate(0, 0, false), Ok(4));
    assert_eq!(shared.fd_duplicate(0, 0, false), Ok(5));
    FAIL_NEXT.set(true);
    drop(reservation);
    assert!(FAIL_NEXT.replace(false));
    assert!(parent.fd_get(3).is_some());
    assert!(parent.fd_get(4).is_some());
    assert_eq!(shared.fd_duplicate(0, 0, false), Ok(3));
    assert_eq!(shared.fd_duplicate(0, 0, false), Ok(4));
}

#[test]
fn actual_path_descriptor_is_available_to_raw_lookup_and_dup_but_not_io_lookup() {
    let owner = task::TaskControlBlock::root(126);
    task::enter(&owner);
    let path =
        file::OpenFileDescription::device(0, 0x200000, owner.fd_get(0).unwrap().opened_ref())
            .unwrap();
    let reservation = owner.fd_prepare_open(true).unwrap();
    let fd = reservation.descriptors()[0];
    assert!(
        owner.fd_get_raw(fd).is_none(),
        "unpublished path is invisible"
    );
    reservation.publish(path.clone());
    assert!(owner.fd_get(fd).is_none(), "O_PATH has no I/O capability");
    assert!(Arc::ptr_eq(&owner.fd_get_raw(fd).unwrap(), &path));
    assert_eq!(owner.fd_flags(fd), Ok(1));
    let duplicate = owner.fd_duplicate(fd, 0, false).unwrap();
    assert!(owner.fd_get(duplicate).is_none());
    assert_eq!(owner.fd_flags(duplicate), Ok(0));
    assert!(Arc::ptr_eq(&owner.fd_get_raw(duplicate).unwrap(), &path));
    owner.fd_close(fd).unwrap();
    assert!(Arc::ptr_eq(&owner.fd_get_raw(duplicate).unwrap(), &path));
}

#[test]
fn actual_two_descriptor_io_capture_rejects_path_and_releases_table_before_operation() {
    let owner = task::TaskControlBlock::root(121);
    task::enter(&owner);
    let regular = owner.fd_get(0).unwrap();
    let path = file::OpenFileDescription::device(0, 0x200000, regular.opened_ref()).unwrap();
    let fd = owner.fd_allocate(path, 0, false).unwrap();
    assert!(
        owner
            .with_file_descriptions(0, fd, |_, _| panic!("O_PATH has no I/O capability"))
            .is_none()
    );
    assert!(
        owner
            .with_file_descriptions(fd, 0, |_, _| panic!("O_PATH has no I/O capability"))
            .is_none()
    );
    owner
        .with_file_descriptions(0, 1, |first, second| {
            assert!(owner.handle_unlocked());
            // 捕获后重用原 FD：operation 仍使用旧 OFD，且表可重入。
            owner.fd_duplicate_to(2, 0, false).unwrap();
            assert!(Arc::ptr_eq(&first, &regular));
            assert!(!Arc::ptr_eq(&owner.fd_get(0).unwrap(), &first));
            assert!(Arc::ptr_eq(&owner.fd_get(1).unwrap(), &second));
        })
        .unwrap();
}
