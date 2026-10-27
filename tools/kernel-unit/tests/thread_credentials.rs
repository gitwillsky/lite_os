#![feature(allocator_ext)]
#![allow(dead_code)]
//! Production Thread credential/group/fs-context transitions and syscall codec.
//! Filesystem object identity, signal security, constructors and user-copy are explicit host seams.
extern crate alloc;
#[path = "../src/sync.rs"]
mod sync;

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

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::{Cell, RefCell},
    sync::Arc,
};
thread_local! {
    static FAIL_NEXT: Cell<bool> = const { Cell::new(false) };
    static PROBE_UNLOCKED: Cell<bool> = const { Cell::new(false) };
}
struct FaultAllocator;
// SAFETY: successful allocations and all deallocations delegate unchanged to System.
unsafe impl GlobalAlloc for FaultAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if FAIL_NEXT
            .try_with(|flag| flag.replace(false))
            .unwrap_or(false)
        {
            PROBE_UNLOCKED.with(|value| {
                value.set(task::current_task().is_none_or(|task| task.credential_unlocked()))
            });
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

#[path = "../../../kernel/src/fs"]
mod fs {
    pub(crate) mod chardev {
        pub(crate) enum DeviceError {
            Errno(isize),
        }
    }
    pub(crate) use crate::mount_membership_fixture::MountNamespace;
    pub(crate) struct OpenedFile(pub(crate) usize);
    pub(crate) struct Vfs;
    pub(crate) fn vfs() -> Vfs {
        Vfs
    }
    impl Vfs {
        pub(crate) fn initial_namespace(&self) -> alloc::sync::Arc<MountNamespace> {
            crate::mount_membership_fixture::initial()
        }
        pub(crate) fn namespace_root(
            &self,
            _: &alloc::sync::Arc<MountNamespace>,
        ) -> Result<alloc::sync::Arc<OpenedFile>, FileSystemError> {
            Ok(alloc::sync::Arc::new(OpenedFile(1)))
        }
        pub(crate) fn is_namespace_root(
            &self,
            _: &alloc::sync::Arc<MountNamespace>,
            root: &alloc::sync::Arc<OpenedFile>,
        ) -> bool {
            root.0 == 1
        }
    }
    #[path = "vfs/lookup_origin.rs"]
    mod lookup_origin;
    pub(crate) use lookup_origin::LookupOrigin;
    pub(crate) enum ProcDirectory {
        Root,
        Cwd,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum InodeType {
        File,
        Directory,
    }
    #[derive(Clone, Copy)]
    pub(crate) struct InodeMetadata {
        pub(crate) kind: InodeType,
        pub(crate) mode: u32,
        pub(crate) uid: u32,
        pub(crate) gid: u32,
    }
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum FileSystemError {
        AccessDenied,
        InvalidOperation,
        PermissionDenied,
    }
    mod permission;
    pub(crate) use permission::AccessIdentity;
    mod procfs {
        mod snapshot;
        pub(crate) use snapshot::ProcCredentialSnapshot;
    }
    pub(crate) use procfs::ProcCredentialSnapshot;
}
mod tty {
    pub(crate) struct Terminal;
    impl Terminal {
        pub(crate) fn proc_identity(&self, _: &crate::namespace::PidNamespace) -> (u32, isize) {
            (0, -1)
        }
    }
}
#[path = "../../../kernel/src/memory"]
mod memory {
    #[path = "../../../tools/kernel-unit/src/shm_backing_fixture.rs"]
    mod backing_input;
    pub(crate) use backing_input::{PAGE_SIZE, SharedMemoryBacking};
    pub(crate) struct InitialStackCredentials {
        pub(crate) uid: u32,
        pub(crate) euid: u32,
        pub(crate) gid: u32,
        pub(crate) egid: u32,
        pub(crate) secure: bool,
    }
    #[derive(Debug)]
    pub(crate) enum MemoryError {
        OutOfMemory,
    }
}
#[path = "../../../kernel/src/task"]
mod task {
    use super::{Arc, RefCell};
    thread_local! { static CURRENT: RefCell<Option<Arc<model::TaskControlBlock>>> = const { RefCell::new(None) }; }
    pub(crate) use model::CapabilityError;
    pub(crate) use model::{CredentialUpdateError, TaskControlBlock};
    pub(crate) fn capability_sets(pid: usize) -> Option<syscall_abi::capability::CapabilitySets> {
        let caller = current_task()?;
        (pid == 0 || pid == caller.tid()).then(|| caller.capability_snapshot().sets)
    }
    pub(crate) fn current_task() -> Option<Arc<TaskControlBlock>> {
        CURRENT.with(|task| task.borrow().clone())
    }
    pub(crate) fn enter(task: &Arc<TaskControlBlock>) {
        CURRENT.with(|current| *current.borrow_mut() = Some(task.clone()));
    }
    pub(crate) mod process_table {
        pub(crate) mod ptrace {
            pub(crate) fn restrict_exec(_: &super::super::TaskControlBlock) -> bool {
                false
            }
        }
        // Graph membership is an explicit fixture seam; actual single-thread check runs in guest.
        pub(crate) fn is_single_threaded(_: &super::TaskControlBlock) -> bool {
            true
        }
        static CREATION: spin::Mutex<()> = spin::Mutex::new(());
        pub(crate) struct CredentialTransition<'a>(&'a super::TaskControlBlock);
        impl CredentialTransition<'_> {
            pub(crate) fn clear_parent_death(&mut self) {
                self.0.clear_parent_death();
            }
        }
        // Graph/PDEATH count is an explicit host seam; the real graph transaction runs in guest.
        pub(crate) fn with_credential_transition<T>(
            task: &super::TaskControlBlock,
            commit: impl FnOnce(&mut CredentialTransition<'_>) -> T,
        ) -> T {
            let _creation = CREATION.lock();
            commit(&mut CredentialTransition(task))
        }
    }
    pub(crate) mod model {
        use alloc::sync::Arc;
        use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use spin::Mutex;
        mod capabilities;
        mod credentials;
        mod message_identity;
        pub(crate) use message_identity::MessageIdentity;
        mod thread_capabilities;
        mod address_space {
            mod dumpability;
            pub(super) struct AddressSpace {
                dumpability: dumpability::Dumpability,
                user_namespace: alloc::sync::Arc<crate::namespace::UserNamespace>,
            }
            impl AddressSpace {
                pub(super) fn user_namespace(
                    &self,
                ) -> &alloc::sync::Arc<crate::namespace::UserNamespace> {
                    &self.user_namespace
                }
                pub(super) fn new(
                    user: bool,
                    user_namespace: alloc::sync::Arc<crate::namespace::UserNamespace>,
                ) -> Self {
                    Self {
                        dumpability: dumpability::Dumpability::new(user),
                        user_namespace,
                    }
                }
                pub(super) fn dumpable(&self) -> bool {
                    self.dumpability.user_dumpable()
                }
                pub(super) fn set_dumpable(&self, user: bool) {
                    self.dumpability.set_user_dumpable(user);
                }
            }
        }
        use address_space::AddressSpace;
        mod thread_mm {
            use super::AddressSpace;
            mod membership;
            pub(super) use membership::MmMembership;
        }
        use thread_mm::MmMembership;
        type MmHandle = Arc<Mutex<MmMembership>>;
        pub(crate) use capabilities::CapabilityError;
        pub(crate) use credentials::CredentialUpdateError;
        use credentials::Credentials;
        use credentials::LeaderCredentialsHandle;
        mod namespaces;
        mod process_resources;
        use namespaces::FsNamespaceHandle;
        use process_resources::FsContext;
        struct Security(AtomicBool);
        impl Security {
            fn enable_no_new_privileges(&self) {
                self.0.store(true, Ordering::Release);
            }
            fn no_new_privileges(&self) -> bool {
                self.0.load(Ordering::Acquire)
            }
        }
        struct Thread {
            mm: MmHandle,
            credentials: Arc<Mutex<Credentials>>,
            security: Security,
            fs_namespaces: FsNamespaceHandle,
        }
        struct ProcessId(usize);
        impl ProcessId {
            fn key(&self) -> usize {
                self.0
            }
        }
        pub(crate) struct Process {
            leader_mm: Mutex<MmHandle>,
            tgid: ProcessId,
            leader_credentials: LeaderCredentialsHandle,
            terminal: Mutex<Option<Arc<crate::tty::Terminal>>>,
        }
        impl Process {
            fn process_address_space(&self) -> Option<Arc<AddressSpace>> {
                self.leader_mm.lock().lock().live().cloned()
            }
            fn process_memory_inspection(&self) -> MmMembership {
                self.leader_mm.lock().lock().clone()
            }
        }
        struct Execution {
            tid: core::sync::atomic::AtomicUsize,
        }
        // Execution kind 是显式构造 seam；credential projection/mutation 使用生产实现。
        struct KernelCredentials {
            credentials: Credentials,
            namespaces: Mutex<Option<namespaces::NamespaceMembership>>,
        }
        enum TaskKind {
            User {},
            Kernel(KernelCredentials),
        }
        pub(crate) struct TaskControlBlock {
            kind: TaskKind,
            thread: Thread,
            process: Arc<Process>,
            execution: Execution,
            parent_death: AtomicUsize,
            user_memory: Mutex<Option<(usize, usize)>>,
        }
        #[test]
        fn actual_kernel_credentials_project_root_and_enforce_signal_dac() {
            let mut kernel = TaskControlBlock::root_with_pid(65_000);
            Arc::get_mut(&mut kernel).unwrap().kind = TaskKind::Kernel(KernelCredentials {
                credentials: Credentials::root(),
                namespaces: Mutex::new(Some(namespaces::initial_membership())),
            });
            let facts = kernel.credential_snapshot();
            assert_eq!(facts.uids, [0; 3]);
            assert_eq!(facts.gids, [0; 3]);
            assert_eq!(facts.fsids, [0; 2]);
            assert!(Arc::ptr_eq(
                &facts.user_namespace,
                &crate::namespace::initial_user()
            ));
            assert_eq!(
                facts.capabilities.sets.effective,
                syscall_abi::capability::VALID_MASK
            );
            assert_eq!(kernel.proc_inode_owner(0o40555), (0, 0));
            let authority = kernel.namespace_authority();
            assert!(Arc::ptr_eq(&authority.namespace, &facts.user_namespace));
            assert_eq!(authority.effective_uid, 0);
            assert_eq!(authority.effective_gid, 0);
            let caller = TaskControlBlock::root_with_pid(65_001);
            assert!(caller.may_signal_thread(&kernel));
            caller.set_credential_id(true, 1000).unwrap();
            assert!(!caller.may_signal_thread(&kernel));
            assert_eq!(kernel.credential_snapshot().uids, [0; 3]);
        }
        impl TaskControlBlock {
            pub(crate) fn signal_allowed_for_test(&self, target: &Process) -> bool {
                self.may_signal_process(target)
            }
            pub(crate) fn retire_namespaces_for_test(&self) {
                self.retire_namespaces();
            }
            pub(crate) fn tgid(&self) -> usize {
                self.process.tgid.0
            }
            pub(crate) fn tid(&self) -> usize {
                self.execution
                    .tid
                    .load(core::sync::atomic::Ordering::Acquire)
            }
            pub(crate) fn visible_tid(&self) -> u32 {
                self.tid() as u32
            }
            pub(crate) fn pid_namespace(&self) -> Arc<crate::namespace::PidNamespace> {
                crate::namespace::initial_pid()
            }
            // 这里只装配 fs/credential namespace；SEM_UNDO 由实际 ipc owner fixture 与 guest 覆盖。
            fn replace_semaphore_undo(&self, _: crate::ipc::SemaphoreUndoHandle) {
                unreachable!("SEM_UNDO lifecycle is outside this producer seam")
            }
            fn thread(&self) -> &Thread {
                &self.thread
            }
            pub(crate) fn process(&self) -> &Arc<Process> {
                &self.process
            }
            fn is_kernel_thread(&self) -> bool {
                matches!(self.kind, TaskKind::Kernel(_))
            }
            pub(crate) fn clear_parent_death(&self) {
                self.parent_death.store(0, Ordering::Release);
            }
            pub(crate) fn root() -> Arc<Self> {
                Self::root_with_pid(1)
            }
            pub(crate) fn root_with_pid(pid: usize) -> Arc<Self> {
                crate::namespace::boot();
                crate::ipc::initialize_namespaces().unwrap();
                crate::socket::boot_net();
                let credentials = Arc::new(Mutex::new(Credentials::root()));
                let mm = Arc::new(Mutex::new(MmMembership::Live(Arc::new(AddressSpace::new(
                    true,
                    crate::namespace::initial_user(),
                )))));
                Arc::new(Self {
                    kind: TaskKind::User {},
                    thread: Thread {
                        mm: mm.clone(),
                        credentials: credentials.clone(),
                        security: Security(false.into()),
                        fs_namespaces: namespaces::initial_fs_namespace_handle(Arc::new(
                            Mutex::new(FsContext {
                                cwd: Arc::new(crate::fs::OpenedFile(1)),
                                root: Arc::new(crate::fs::OpenedFile(1)),
                                umask: process_resources::DEFAULT_UMASK,
                            }),
                        ))
                        .unwrap(),
                    },
                    process: Arc::new(Process {
                        leader_mm: Mutex::new(mm),
                        tgid: ProcessId(pid),
                        leader_credentials: Arc::new(Mutex::new(credentials)),
                        terminal: Mutex::new(None),
                    }),
                    execution: Execution {
                        tid: core::sync::atomic::AtomicUsize::new(pid),
                    },
                    parent_death: 9.into(),
                    user_memory: Mutex::new(None),
                })
            }
            pub(crate) fn sibling(&self) -> Arc<Self> {
                self.sibling_with_fs(true)
            }
            pub(crate) fn sibling_with_fs(&self, share: bool) -> Arc<Self> {
                let inherited = self.thread.credentials.lock().clone();
                Arc::new(Self {
                    kind: TaskKind::User {},
                    thread: Thread {
                        mm: Arc::new(Mutex::new(self.thread.mm.lock().clone())),
                        credentials: Arc::new(Mutex::new(inherited)),
                        security: Security(self.no_new_privileges().into()),
                        fs_namespaces: namespaces::try_fs_namespace_handle(
                            self.namespace_snapshot(),
                            self.clone_fs_context(share).unwrap(),
                        )
                        .unwrap(),
                    },
                    process: self.process.clone(),
                    execution: Execution {
                        tid: core::sync::atomic::AtomicUsize::new(self.tid() + 1),
                    },
                    parent_death: 9.into(),
                    user_memory: Mutex::new(None),
                })
            }
            // Process limits constructor seam；实际 rlimit mutation 由 resource-limit guest 覆盖。
            fn namespace_rlimits(&self) -> crate::namespace::NamespaceRlimits {
                crate::namespace::NamespaceRlimits {
                    locked_memory: u64::MAX,
                    pending_signals: u64::MAX,
                }
            }
            fn mm_snapshot(&self) -> Option<Arc<AddressSpace>> {
                self.thread.mm.lock().live().cloned()
            }
            fn memory_inspection(&self) -> MmMembership {
                if self.is_kernel_thread() {
                    MmMembership::Retired {
                        user_dumpable: false,
                    }
                } else {
                    self.thread.mm.lock().clone()
                }
            }
            fn address_space(&self) -> Arc<AddressSpace> {
                self.mm_snapshot().unwrap()
            }
            fn process_address_space(&self) -> Option<Arc<AddressSpace>> {
                self.process.leader_mm.lock().lock().live().cloned()
            }
            pub(crate) fn user_dumpable(&self) -> bool {
                self.address_space().dumpable()
            }
            pub(crate) fn set_user_dumpable(&self, user: bool) {
                self.address_space().set_dumpable(user);
            }
            pub(crate) fn observed_inode_owner(&self, mode: u32) -> (u32, u32) {
                self.proc_inode_owner(mode)
            }
            fn new_exec_mm(&self) {
                let replacement = Arc::new(AddressSpace::new(false, self.user_namespace()));
                let retired = self.thread.mm.lock().replace_live(replacement);
                drop(retired);
                *self.process.leader_mm.lock() = self.thread.mm.clone();
            }
            pub(crate) fn observed_credentials(&self) -> crate::fs::ProcCredentialSnapshot {
                self.credential_snapshot()
            }
            pub(crate) fn observed_process_credentials(&self) -> crate::fs::ProcCredentialSnapshot {
                self.process().process_credential_snapshot()
            }
            pub(crate) fn observed_robust_access(&self, target: &Self) -> bool {
                self.may_inspect_thread_real(target)
            }
            pub(crate) fn retire_mm_for_inspection(&self) -> bool {
                let weak = Arc::downgrade(self.thread.mm.lock().live().unwrap());
                drop(self.thread.mm.lock().retire());
                weak.upgrade().is_none()
            }
            pub(crate) fn credential_unlocked(&self) -> bool {
                self.thread.credentials.try_lock().is_some()
                    && self.thread.fs_namespaces.try_lock().is_some_and(|state| {
                        state
                            .fs
                            .as_ref()
                            .is_none_or(|context| context.try_lock().is_some())
                    })
            }
            // Files owner 单独由 file-table fixture 执行；此 fixture 只请求 fs 脱离。
            fn prepare_file_table_unshare(
                &self,
            ) -> Result<Option<usize>, crate::memory::MemoryError> {
                unreachable!("files seam")
            }
            fn commit_file_table_unshare(&self, prepared: Option<usize>) {
                assert!(prepared.is_none());
            }
            pub(crate) fn exit_fs(&self) {
                self.retire_fs_context();
            }
            pub(crate) fn pdeath(&self) -> usize {
                self.parent_death.load(Ordering::Acquire)
            }
            pub(crate) fn exec_setid(&self, mode: u32, uid: u32, gid: u32) {
                let prepared = self.prepare_exec_credentials(mode, uid, gid, None).unwrap();
                self.new_exec_mm();
                self.commit_exec_credentials(prepared, true);
            }
            pub(crate) fn exec_facts(
                &self,
                mode: u32,
                uid: u32,
                gid: u32,
            ) -> crate::memory::InitialStackCredentials {
                let prepared = self.prepare_exec_credentials(mode, uid, gid, None).unwrap();
                let facts = prepared.stack_credentials();
                self.new_exec_mm();
                self.commit_exec_credentials(prepared, true);
                facts
            }
            pub(crate) fn arm_pdeath(&self) {
                self.parent_death.store(9, Ordering::Release);
            }
            pub(crate) fn scheduler_permission(&self, target: &Self) -> Option<bool> {
                self.scheduler_privilege_for(target)
            }
            pub(crate) fn proc_thread_permission(&self, target: &Self) -> bool {
                self.may_inspect_thread(target)
            }
            pub(crate) fn proc_permission(&self, target: &Self) -> bool {
                self.may_inspect_process(target.process())
            }
            pub(crate) fn memory(&self, bytes: &mut [u8]) {
                *self.user_memory.lock() = Some((bytes.as_mut_ptr() as usize, bytes.len()));
            }
            fn valid_memory(&self, address: usize, length: usize) -> bool {
                self.user_memory.lock().is_some_and(|(start, capacity)| {
                    address >= start
                        && address
                            .checked_add(length)
                            .zip(start.checked_add(capacity))
                            .is_some_and(|(end, limit)| end <= limit)
                })
            }
            pub(crate) fn copy_from_user(
                &self,
                address: usize,
                bytes: &mut [u8],
            ) -> Result<(), ()> {
                if !self.valid_memory(address, bytes.len()) {
                    return Err(());
                }
                // SAFETY: the test caller retains the registered live slice through the syscall.
                bytes.copy_from_slice(unsafe {
                    core::slice::from_raw_parts(address as *const u8, bytes.len())
                });
                Ok(())
            }
            pub(crate) fn copy_to_user(&self, address: usize, bytes: &[u8]) -> Result<(), ()> {
                if !self.valid_memory(address, bytes.len()) {
                    return Err(());
                }
                // SAFETY: the test caller retains exclusive ownership of the registered live slice.
                unsafe { core::slice::from_raw_parts_mut(address as *mut u8, bytes.len()) }
                    .copy_from_slice(bytes);
                Ok(())
            }
        }
    }
}
mod syscall {
    pub(crate) mod errno {
        pub(crate) const EPERM: isize = 1;
        pub(crate) const EFAULT: isize = 14;
        pub(crate) const EINVAL: isize = 22;
        pub(crate) const ENOMEM: isize = 12;
        pub(crate) const ESRCH: isize = 3;
    }
}
#[path = "../../../kernel/src/syscall/capabilities.rs"]
mod capability_syscall;
#[path = "../../../kernel/src/syscall/credentials.rs"]
mod credential_syscall;

#[test]
fn scheduler_and_proc_use_permitted_and_effective_subset_respectively() {
    use syscall_abi::capability::{Capability, CapabilitySets, VALID_MASK};
    let target = task::TaskControlBlock::root_with_pid(82);
    let caller = task::TaskControlBlock::root_with_pid(81);
    caller.set_capabilities(CapabilitySets::default()).unwrap();
    assert_eq!(caller.scheduler_permission(&target), None);
    assert!(!caller.proc_permission(&target));
    let caller = task::TaskControlBlock::root_with_pid(83);
    caller
        .set_capabilities(CapabilitySets {
            effective: 0,
            permitted: VALID_MASK,
            inheritable: 0,
        })
        .unwrap();
    assert_eq!(caller.scheduler_permission(&target), Some(false));
    assert!(!caller.proc_permission(&target));
    let caller = task::TaskControlBlock::root_with_pid(84);
    caller.capability_prctl(28, 4, [0; 3]).unwrap();
    caller.set_credential_id(true, 1000).unwrap();
    let caps = Capability::SYS_NICE.mask() | Capability::SYS_PTRACE.mask();
    caller
        .set_capabilities(CapabilitySets {
            effective: caps,
            permitted: caps,
            inheritable: 0,
        })
        .unwrap();
    assert_eq!(caller.scheduler_permission(&target), Some(true));
    assert!(caller.proc_permission(&target));
    let sibling = caller.sibling();
    caller.set_capabilities(CapabilitySets::default()).unwrap();
    assert!(caller.proc_permission(&sibling));
}

#[test]
fn exec_prepared_ids_ambient_nnp_secure_and_parent_death_commit_together() {
    use syscall_abi::capability::{Capability, CapabilitySets, VALID_MASK};
    let root = task::TaskControlBlock::root();
    root.set_capabilities(CapabilitySets::default()).unwrap();
    let facts = root.exec_facts(0o755, 0, 0);
    assert!(!facts.secure);
    assert_eq!(root.capability_snapshot().sets.permitted, VALID_MASK);
    assert_eq!(root.pdeath(), 0);
    let root = task::TaskControlBlock::root();
    root.set_credential_res_ids(true, [1000, 0, 0]).unwrap();
    root.set_capabilities(CapabilitySets::default()).unwrap();
    root.enable_no_new_privileges();
    let facts = root.exec_facts(0o755, 0, 0);
    assert_eq!([facts.uid, facts.euid], [1000; 2]);
    assert!(facts.secure);
    assert_eq!(root.credential_res_ids(true), [1000; 3]);
    let user = task::TaskControlBlock::root();
    user.set_credential_id(false, 1000).unwrap();
    user.set_credential_id(true, 1000).unwrap();
    user.arm_pdeath();
    let facts = user.exec_facts(0o6745, 1000, 2000);
    assert_eq!([facts.gid, facts.egid], [1000; 2]);
    assert!(!facts.secure);
    assert_eq!(user.pdeath(), 9);
    let user = task::TaskControlBlock::root();
    let keep = Capability::NET_BIND_SERVICE.mask();
    user.set_supplementary_groups(vec![2000]).unwrap();
    user.set_credential_id(false, 1000).unwrap();
    user.capability_prctl(8, 1, [0; 3]).unwrap();
    user.set_capabilities(CapabilitySets {
        effective: VALID_MASK,
        permitted: VALID_MASK,
        inheritable: keep,
    })
    .unwrap();
    user.set_credential_id(true, 1000).unwrap();
    user.set_capabilities(CapabilitySets {
        effective: 0,
        permitted: keep,
        inheritable: keep,
    })
    .unwrap();
    user.capability_prctl(47, 2, [10, 0, 0]).unwrap();
    let facts = user.exec_facts(0o2755, 1000, 2000);
    assert_eq!([facts.gid, facts.egid], [1000, 2000]);
    assert!(facts.secure);
    assert_eq!(user.capability_snapshot().ambient, keep);
    assert_eq!(user.capability_snapshot().sets.effective, keep);
}

#[test]
fn inode_dac_and_sticky_policy_are_capability_based() {
    use syscall_abi::capability::Capability;
    let file = fs::InodeMetadata {
        kind: fs::InodeType::File,
        mode: 0,
        uid: 2000,
        gid: 2000,
    };
    let root = fs::AccessIdentity::new(0, 0, None, crate::namespace::boot(), 0, 0);
    assert!(!root.permits(file, 4));
    let dac = fs::AccessIdentity::new(
        1000,
        1000,
        None,
        crate::namespace::boot(),
        Capability::DAC_OVERRIDE.mask(),
        1000,
    );
    assert!(dac.permits(file, 6));
    assert!(!dac.permits(file, 1));
    assert!(dac.permits(
        fs::InodeMetadata {
            mode: 0o001,
            ..file
        },
        1
    ));
    let directory = fs::InodeMetadata {
        kind: fs::InodeType::Directory,
        ..file
    };
    assert!(dac.permits(directory, 7));
    let search = fs::AccessIdentity::new(
        1000,
        1000,
        None,
        crate::namespace::boot(),
        Capability::DAC_READ_SEARCH.mask(),
        1000,
    );
    assert!(search.permits(file, 4));
    assert!(!search.permits(file, 6));
    assert!(search.permits(directory, 5));
    assert!(!search.permits(directory, 2));
    let sticky = fs::InodeMetadata {
        mode: 0o1777,
        ..directory
    };
    assert!(!root.may_remove_entry(sticky, 3000, 2000));
    let fowner = fs::AccessIdentity::new(
        1000,
        1000,
        None,
        crate::namespace::boot(),
        Capability::FOWNER.mask(),
        1000,
    );
    assert!(fowner.may_remove_entry(sticky, 3000, 2000));
    assert!(root.may_remove_entry(sticky, 0, 2000));
}

#[test]
fn production_capability_syscall_preserves_version_pid_and_copy_error_order() {
    use syscall_abi::capability::{CapabilitySets, VERSION_3};
    let caller = task::TaskControlBlock::root();
    task::enter(&caller);
    let mut bytes = [0u8; 32];
    let header = bytes.as_mut_ptr() as usize;
    caller.memory(&mut bytes[..4]); // valid version word, inaccessible PID
    assert_eq!(capability_syscall::sys_capget(header, 0), 0);
    assert_eq!(&bytes[..4], &VERSION_3.to_ne_bytes());
    assert_eq!(capability_syscall::sys_capget(header, 0), 0);
    assert_eq!(
        capability_syscall::sys_capset(header, 0),
        -syscall::errno::EFAULT
    );
    bytes[..4].fill(0);
    assert_eq!(
        capability_syscall::sys_capset(header, 0),
        -syscall::errno::EINVAL
    );
    assert_eq!(&bytes[..4], &VERSION_3.to_ne_bytes());
    caller.memory(&mut bytes[..8]);
    bytes[4..8].copy_from_slice(&77i32.to_ne_bytes());
    assert_eq!(
        capability_syscall::sys_capset(header, 0),
        -syscall::errno::EPERM
    );
    bytes[4..8].copy_from_slice(&(-1i32).to_ne_bytes());
    assert_eq!(
        capability_syscall::sys_capget(header, header + 8),
        -syscall::errno::EINVAL
    );
    bytes[4..8].fill(0);
    let old = caller.capability_snapshot();
    assert_eq!(
        capability_syscall::sys_capset(header, header + 8),
        -syscall::errno::EFAULT
    );
    assert_eq!(caller.capability_snapshot(), old);
    caller.memory(&mut bytes);
    bytes[8..].copy_from_slice(
        &CapabilitySets {
            effective: 1,
            permitted: 0,
            inheritable: 0,
        }
        .encode(),
    );
    assert_eq!(
        capability_syscall::sys_capset(header, header + 8),
        -syscall::errno::EPERM
    );
    assert_eq!(caller.capability_snapshot(), old);
    bytes[8..].copy_from_slice(
        &CapabilitySets {
            effective: 1,
            permitted: 1,
            inheritable: 0,
        }
        .encode(),
    );
    assert_eq!(capability_syscall::sys_capset(header, header + 8), 0);
    assert_eq!(caller.capability_snapshot().sets.permitted, 1);
    bytes[8..].fill(0);
    assert_eq!(capability_syscall::sys_capget(header, header + 8), 0);
    assert_eq!(CapabilitySets::decode(&bytes[8..]).unwrap().effective, 1);
}

#[test]
fn raw_credential_transition_does_not_change_siblings_or_shared_fs_context() {
    let parent = task::TaskControlBlock::root();
    parent.set_supplementary_groups(vec![41, 3, 41, 4]).unwrap();
    let sibling = parent.sibling();
    assert_eq!(parent.replace_umask(0o077), 0o022);
    assert_eq!(sibling.creation_mode(0o777), 0o700);
    sibling.set_supplementary_groups(vec![9]).unwrap();
    sibling.set_credential_id(false, 1001).unwrap();
    sibling.set_credential_id(true, 1000).unwrap();
    assert_eq!(parent.credential_res_ids(true), [0; 3]);
    assert_eq!(parent.credential_res_ids(false), [0; 3]);
    assert_eq!(sibling.credential_res_ids(true), [1000; 3]);
    assert_eq!(sibling.credential_res_ids(false), [1001; 3]);
    assert_eq!(
        parent.supplementary_groups().unwrap().as_slice(),
        &[3, 4, 41, 41]
    );
    assert_eq!(sibling.supplementary_groups().unwrap().as_slice(), &[9]);
    assert_eq!(sibling.creation_mode(0o666), 0o600);
    assert_eq!(sibling.pdeath(), 0);
    assert_eq!(parent.pdeath(), 9);
    assert!(sibling.may_signal_thread(&parent));
    assert!(sibling.signal_allowed_for_test(parent.process()));
    assert!(parent.may_signal_thread(&sibling));
    let other = task::TaskControlBlock::root_with_pid(7);
    assert!(!sibling.may_signal_thread(&other));
    assert!(!sibling.signal_allowed_for_test(other.process()));
}

#[test]
fn fs_context_sharing_detachment_and_oom_are_one_owner_transaction() {
    let parent = task::TaskControlBlock::root();
    task::enter(&parent);
    let shared = parent.sibling();
    let copied = parent.sibling_with_fs(false);
    shared.set_working_directory(Arc::new(fs::OpenedFile(3)));
    assert_eq!(parent.lookup_origin().into_parts().1.0, 3);
    assert_eq!(copied.lookup_origin().into_parts().1.0, 1);
    assert_eq!(parent.replace_umask(0o077), 0o022);
    assert_eq!(shared.creation_mode(0o777), 0o700);
    assert_eq!(copied.creation_mode(0o777), 0o755);
    FAIL_NEXT.set(true);
    assert!(
        parent
            .unshare_namespaces(
                true, false, false, false, false, false, false, false, false, false
            )
            .is_err()
    );
    assert!(PROBE_UNLOCKED.get());
    assert_eq!(shared.replace_umask(0o033), 0o077);
    assert_eq!(parent.creation_mode(0o777), 0o744);
    parent
        .unshare_namespaces(
            true, false, false, false, false, false, false, false, false, false,
        )
        .unwrap();
    shared.set_working_directory(Arc::new(fs::OpenedFile(4)));
    shared.replace_umask(0o077);
    assert_eq!(parent.lookup_origin().into_parts().1.0, 3);
    assert_eq!(parent.creation_mode(0o777), 0o744);
    FAIL_NEXT.set(true);
    parent
        .unshare_namespaces(
            true, false, false, false, false, false, false, false, false, false,
        )
        .unwrap();
    assert!(FAIL_NEXT.replace(false));
}

#[test]
fn fs_root_shares_copies_detaches_and_retires_with_the_same_context_owner() {
    let parent = task::TaskControlBlock::root();
    let shared = parent.sibling();
    let copied = parent.sibling_with_fs(false);
    let root = Arc::new(fs::OpenedFile(8));
    let retired = Arc::downgrade(&root);
    shared.set_root_directory(root);
    assert_eq!(parent.lookup_origin().root().0, 8);
    assert_eq!(copied.lookup_origin().root().0, 1);
    assert_eq!(parent.lookup_origin().into_parts().1.0, 1); // chroot leaves cwd untouched
    parent
        .unshare_namespaces(
            true, false, false, false, false, false, false, false, false, false,
        )
        .unwrap();
    shared.set_root_directory(Arc::new(fs::OpenedFile(9)));
    assert_eq!(parent.lookup_origin().root().0, 8);
    assert_eq!(shared.lookup_origin().root().0, 9);
    parent.exit_fs();
    assert!(retired.upgrade().is_none()); // TCB observer cannot pin root
}

#[test]
fn exited_tcb_observer_does_not_retain_fs_owner_or_shared_reference_count() {
    let parent = task::TaskControlBlock::root();
    let private = parent.sibling_with_fs(false);
    let directory = Arc::new(fs::OpenedFile(3));
    let directory_weak = Arc::downgrade(&directory);
    private.set_working_directory(directory);
    private.exit_fs();
    assert!(directory_weak.upgrade().is_none());
    // private TCB 此时仍由测试观察者保活，必须已经不持有 cwd。
    assert_eq!(private.credential_res_ids(true), [0; 3]);
    let shared = parent.sibling();
    shared.exit_fs();
    assert_eq!(parent.lookup_origin().into_parts().1.0, 1);
    FAIL_NEXT.set(true);
    parent
        .unshare_namespaces(
            true, false, false, false, false, false, false, false, false, false,
        )
        .unwrap();
    assert!(FAIL_NEXT.replace(false));
}

#[test]
fn invalid_ids_groups_sorted_duplicate_projection_and_saved_id_rules_are_atomic() {
    use task::CredentialUpdateError as Error;
    let parent = task::TaskControlBlock::root();
    assert_eq!(
        parent.set_credential_id(true, u32::MAX),
        Err(Error::Invalid)
    );
    assert_eq!(
        parent.set_credential_id(false, u32::MAX),
        Err(Error::Invalid)
    );
    assert_eq!(parent.credential_res_ids(true), [0; 3]);
    assert_eq!(parent.pdeath(), 9);
    parent.set_supplementary_groups(vec![10, 2, 10]).unwrap();
    assert_eq!(
        parent.set_supplementary_groups(vec![1, u32::MAX]),
        Err(Error::Invalid)
    );
    assert_eq!(
        parent.supplementary_groups().unwrap().as_slice(),
        &[2, 10, 10]
    );
    parent
        .set_credential_res_ids(true, [1000, 0, 2000])
        .unwrap();
    assert_eq!(parent.set_credential_res_ids(true, [u32::MAX; 3]), Ok(()));
    parent.set_credential_id(true, 1000).unwrap();
    assert_eq!(parent.credential_res_ids(true), [1000; 3]);
    assert_eq!(parent.set_credential_id(true, 0), Err(Error::Permission));
}

#[test]
fn proc_thread_directory_permission_uses_actual_target_credentials() {
    let target = task::TaskControlBlock::root_with_pid(7);
    let worker = target.sibling();
    worker.set_credential_id(false, 1000).unwrap();
    worker.set_credential_id(true, 1000).unwrap();
    let observer = task::TaskControlBlock::root_with_pid(9);
    observer.set_credential_id(false, 1000).unwrap();
    observer.set_credential_id(true, 1000).unwrap();
    worker.set_user_dumpable(true);
    assert!(observer.proc_thread_permission(&worker));
    assert!(!observer.proc_permission(&worker));
    assert!(!observer.proc_thread_permission(&target));
}

#[test]
fn filesystem_identity_is_independent_and_commits_security_without_allocation() {
    let caller = task::TaskControlBlock::root();
    task::enter(&caller);
    let sibling = caller.sibling();
    caller.set_supplementary_groups(vec![3, 7]).unwrap();
    caller.set_user_dumpable(true);
    caller.arm_pdeath();
    FAIL_NEXT.set(true);
    let old = credential_syscall::sys_set_fs_id(true, 123);
    let no_allocation = FAIL_NEXT.replace(false);
    assert!(no_allocation);
    assert_eq!(old, 0);
    assert_eq!(caller.credential_res_ids(true), [0; 3]);
    assert_eq!(caller.observed_credentials().fsids, [123, 0]);
    assert_eq!(caller.access_identity(true).uid(), 123);
    assert_eq!(caller.access_identity(false).uid(), 0);
    assert_eq!(sibling.observed_credentials().fsids, [0, 0]);
    assert!(!caller.user_dumpable());
    assert!(!sibling.user_dumpable()); // CLONE_VM actual dumpability leaf is shared; MM membership is a seam.
    assert_eq!(caller.pdeath(), 0);
    assert_eq!(sibling.pdeath(), 9);
    caller.set_user_dumpable(true);
    caller.arm_pdeath();
    assert_eq!(caller.set_filesystem_id(true, u32::MAX), 123);
    assert!(caller.user_dumpable());
    assert_eq!(caller.pdeath(), 9);
    caller.set_credential_res_ids(true, [u32::MAX; 3]).unwrap();
    assert_eq!(caller.observed_credentials().fsids, [0, 0]);
    assert!(!caller.user_dumpable());
    assert_eq!(caller.pdeath(), 0);
    assert_eq!(caller.set_filesystem_id(false, 321), 0);
    assert_eq!(caller.access_identity(true).gid(), 321);
    assert_eq!(caller.credential_id(false, true), 0);
    caller.set_credential_res_ids(false, [u32::MAX; 3]).unwrap();
    assert_eq!(caller.access_identity(true).gid(), 0);
}

#[test]
fn rejected_identity_update_preserves_dumpability_parent_death_and_all_ids() {
    use task::CredentialUpdateError as Error;
    let caller = task::TaskControlBlock::root();
    caller.set_credential_id(true, 1000).unwrap();
    caller.set_user_dumpable(true);
    caller.arm_pdeath();
    let before = caller.observed_credentials();
    assert_eq!(caller.set_filesystem_id(true, 2000), 1000);
    assert_eq!(caller.set_credential_id(true, 2000), Err(Error::Permission));
    assert_eq!(
        caller.set_credential_id(false, u32::MAX),
        Err(Error::Invalid)
    );
    assert!(caller.user_dumpable());
    assert_eq!(caller.pdeath(), 9);
    assert_eq!(caller.observed_credentials().uids, before.uids);
    assert_eq!(caller.observed_credentials().gids, before.gids);
    assert_eq!(caller.observed_credentials().fsids, before.fsids);
}

#[test]
fn exec_fs_only_change_does_not_reenable_dumpability_after_credential_commit() {
    let caller = task::TaskControlBlock::root();
    caller.set_filesystem_id(true, 123);
    caller.set_user_dumpable(true);
    caller.arm_pdeath();
    let facts = caller.exec_facts(0o755, 0, 0);
    assert_eq!(caller.observed_credentials().fsids, [0, 0]);
    assert_eq!((facts.uid, facts.euid), (0, 0));
    assert!(!facts.secure);
    assert!(!caller.user_dumpable());
    assert_eq!(caller.pdeath(), 0);
    caller.exec_facts(0o755, 0, 0);
    assert!(caller.user_dumpable()); // next ordinary exec, no credential effect.
}

#[test]
fn proc_fscreds_uses_caller_fs_ids_and_requires_dumpable_target() {
    use syscall_abi::capability::CapabilitySets;
    let target = task::TaskControlBlock::root_with_pid(7);
    target.set_credential_id(false, 1000).unwrap();
    target.set_credential_id(true, 1000).unwrap();
    let observer = task::TaskControlBlock::root_with_pid(9);
    observer.set_filesystem_id(false, 1000);
    observer.set_filesystem_id(true, 1000);
    observer
        .set_capabilities(CapabilitySets::default())
        .unwrap();
    assert!(!observer.proc_thread_permission(&target));
    target.set_user_dumpable(true);
    assert!(observer.proc_thread_permission(&target));
    observer.set_credential_id(false, 0).unwrap();
    assert!(!observer.proc_thread_permission(&target));
}

#[test]
fn robust_read_uses_real_ids_and_permitted_capabilities_in_the_actual_authorization_owner() {
    use syscall_abi::capability::{CapabilitySets, VALID_MASK};
    let target = task::TaskControlBlock::root_with_pid(101);
    let caller = task::TaskControlBlock::root_with_pid(102);
    caller
        .set_capabilities(CapabilitySets {
            effective: 0,
            permitted: VALID_MASK,
            inheritable: 0,
        })
        .unwrap();
    assert!(!caller.proc_thread_permission(&target));
    assert!(caller.observed_robust_access(&target));
    target.set_credential_id(false, 1000).unwrap();
    target.set_credential_id(true, 1000).unwrap();
    target.set_user_dumpable(true);
    let fs = task::TaskControlBlock::root_with_pid(103);
    fs.set_filesystem_id(false, 1000);
    fs.set_filesystem_id(true, 1000);
    fs.set_capabilities(CapabilitySets::default()).unwrap();
    assert!(fs.proc_thread_permission(&target));
    assert!(!fs.observed_robust_access(&target));
    let real = task::TaskControlBlock::root_with_pid(104);
    real.set_credential_res_ids(false, [1000, u32::MAX, u32::MAX])
        .unwrap();
    real.set_credential_res_ids(true, [1000, u32::MAX, u32::MAX])
        .unwrap();
    real.set_capabilities(CapabilitySets::default()).unwrap();
    assert!(!real.proc_thread_permission(&target));
    assert!(real.observed_robust_access(&target));
    assert!(
        target.retire_mm_for_inspection(),
        "retired aliases cannot pin the old mapping"
    );
    assert!(
        real.observed_robust_access(&target),
        "dumpable exit remains inspectable by matching real IDs"
    );
}

#[test]
fn retired_nondumpable_mm_requires_initial_namespace_ptrace_instead_of_a_missing_mm_bypass() {
    let caller = task::TaskControlBlock::root_with_pid(105);
    let target = task::TaskControlBlock::root_with_pid(106);
    let parent = caller.namespace_authority();
    let child = namespace::UserNamespace::child(
        &parent,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    child
        .publish_map(true, b"0 0 1\n", &parent, &parent)
        .unwrap();
    child
        .publish_map(false, b"0 0 1\n", &parent, &parent)
        .unwrap();
    caller
        .set_namespace(namespace::NamespaceReference::new(child.clone()))
        .unwrap();
    target
        .set_namespace(namespace::NamespaceReference::new(child))
        .unwrap();
    target.exec_setid(0o755, 0, 0);
    target.set_user_dumpable(false);
    assert!(caller.observed_robust_access(&target));
    assert!(target.retire_mm_for_inspection());
    assert!(!caller.observed_robust_access(&target));
    assert!(!caller.proc_thread_permission(&target));
    assert!(!caller.proc_permission(&target));
}

#[test]
fn group_owner_oom_is_outside_credential_lock_and_preserves_all_old_snapshots() {
    use task::CredentialUpdateError as Error;
    let parent = task::TaskControlBlock::root();
    task::enter(&parent);
    parent.set_supplementary_groups(vec![30, 4]).unwrap();
    let before = parent.observed_credentials();
    let identity = parent.access_identity(true);
    let prepared = vec![7, 3];
    FAIL_NEXT.set(true);
    assert_eq!(
        parent.set_supplementary_groups(prepared),
        Err(Error::OutOfMemory)
    );
    assert!(!FAIL_NEXT.get());
    assert!(PROBE_UNLOCKED.get());
    assert_eq!(parent.supplementary_groups().unwrap().as_slice(), &[4, 30]);
    parent.set_supplementary_groups(vec![6]).unwrap();
    assert_eq!(before.groups.unwrap().as_slice(), &[4, 30]);
    assert!(identity.in_group(30));
    assert!(!parent.access_identity(true).in_group(30));
}

#[test]
fn process_leader_alias_survives_leader_drop_and_is_not_a_sibling_credential_owner() {
    let parent = task::TaskControlBlock::root();
    parent.set_supplementary_groups(vec![4]).unwrap();
    let sibling = parent.sibling();
    parent.set_credential_id(true, 42).unwrap();
    drop(parent);
    sibling.set_credential_id(true, 7).unwrap();
    assert_eq!(sibling.observed_credentials().uids, [7; 3]);
    let process = sibling.observed_process_credentials();
    assert_eq!(process.uids, [42; 3]);
    assert_eq!(process.groups.unwrap().as_slice(), &[4]);
}

#[test]
fn exec_setid_and_nnp_use_the_calling_thread_credential_owner() {
    let parent = task::TaskControlBlock::root();
    let sibling = parent.sibling();
    sibling.set_credential_id(false, 1000).unwrap();
    sibling.set_credential_id(true, 1000).unwrap();
    sibling.enable_no_new_privileges();
    sibling.exec_setid(0o6755, 0, 0);
    assert_eq!(sibling.credential_res_ids(true), [1000; 3]);
    assert_eq!(parent.credential_res_ids(true), [0; 3]);
}

#[test]
fn getgroups_query_has_no_allocation_and_codec_honors_int_count_and_permission_order() {
    use credential_syscall::*;
    let parent = task::TaskControlBlock::root();
    task::enter(&parent);
    parent.set_supplementary_groups(vec![10, 3, 10]).unwrap();
    FAIL_NEXT.set(true);
    assert_eq!(sys_getgroups(0, 0), 3);
    assert!(FAIL_NEXT.replace(false));
    assert_eq!(sys_getgroups(usize::MAX, 0), -22);
    assert_eq!(sys_getgroups(0xabcd_1234_0000_0001, 0), -22);
    let mut bytes = [0u8; 12];
    parent.memory(&mut bytes);
    assert_eq!(sys_getgroups(3, bytes.as_mut_ptr() as usize), 3);
    assert_eq!(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| u32::from_ne_bytes(*bytes))
            .collect::<Vec<_>>(),
        vec![3, 10, 10]
    );
    assert_eq!(sys_set_id(true, u32::MAX), -22);
    assert_eq!(sys_setgroups(65_537, 0), -22);
    assert_eq!(sys_setgroups(1, 0), -14);
    let mut invalid = u32::MAX.to_ne_bytes();
    parent.memory(&mut invalid);
    // 第一个 gid 非法、第二个位于不可读区：Linux 的逐元素顺序先返回 EINVAL。
    assert_eq!(sys_setgroups(2, invalid.as_mut_ptr() as usize), -22);
    assert_eq!(
        parent.supplementary_groups().unwrap().as_slice(),
        &[3, 10, 10]
    );
    parent.set_credential_id(true, 1000).unwrap();
    assert_eq!(sys_setgroups(65_537, 0), -1);
    assert_eq!(sys_setgroups(1, 0), -1);
    assert_eq!(sys_setgroups(0, 0), -1);
}

#[test]
#[ignore = "diagnostic CPU measurement; run alone in release"]
fn ordered_group_permission_and_credential_clone_cpu_cost() {
    use std::{hint::black_box, time::Instant};
    fn measure(name: &str, mut operation: impl FnMut()) {
        for _ in 0..100_000 {
            operation();
        }
        let mut samples = [0f64; 5];
        for sample in &mut samples {
            let start = Instant::now();
            for _ in 0..1_000_000 {
                operation();
            }
            *sample = start.elapsed().as_nanos() as f64 / 1_000_000.;
        }
        samples.sort_by(f64::total_cmp);
        println!("{name}: median {:.2} ns", samples[2]);
    }
    for count in [0, 32, 65_536] {
        let parent = task::TaskControlBlock::root();
        parent
            .set_supplementary_groups((1..=count).rev().collect())
            .unwrap();
        let identity = parent.access_identity(true);
        measure(&format!("in_group {count}"), || {
            black_box(identity.in_group(black_box(count)));
        });
        measure(&format!("credential snapshot {count}"), || {
            black_box(parent.observed_credentials());
        });
    }
    let parent = task::TaskControlBlock::root();
    measure("credential UID getter", || {
        black_box(parent.credential_id(true, true));
    });
    measure("credential capability getter", || {
        black_box(parent.has_capability(syscall_abi::capability::Capability::SYS_ADMIN));
    });
    let identity = parent.access_identity(true);
    measure("content write kill-mask", || {
        black_box(identity.write_kill_mask(fs::InodeType::File, black_box(0o6777), 1000));
    });
    measure("fs context coherent root/cwd snapshot", || {
        black_box(parent.lookup_origin().into_parts().1);
    });
    measure("fs context creation mode", || {
        black_box(parent.creation_mode(black_box(0o777)));
    });
}

#[test]
fn actual_user_namespace_identity_projection_and_capabilities_never_authorize_global_inodes() {
    use syscall_abi::capability::Capability;
    let caller = task::TaskControlBlock::root();
    let parent = caller.namespace_authority();
    caller
        .unshare_namespaces(
            false, false, true, false, false, false, false, false, false, false,
        )
        .unwrap();
    let namespace = caller.user_namespace();
    assert!(!Arc::ptr_eq(&namespace, &parent.namespace));
    assert_eq!(caller.visible_credential_id(true, true), 65534);
    assert!(caller.user_dumpable());
    assert_eq!(caller.pdeath(), 9);
    namespace
        .publish_map(true, b"0 1000 1\n", &parent, &parent)
        .unwrap();
    namespace
        .publish_map(false, b"0 1001 1\n", &parent, &parent)
        .unwrap();
    caller.set_credential_id(true, 0).unwrap();
    caller.set_credential_id(false, 0).unwrap();
    assert_eq!(caller.credential_id(true, true), 1000);
    assert_eq!(caller.visible_credential_res_ids(true), [0; 3]);
    assert_eq!(caller.visible_credential_id(false, true), 0);
    assert!(caller.has_capability(Capability::SYS_ADMIN));
    assert!(!caller.has_initial_capability(Capability::SYS_ADMIN));
    let identity = caller.access_identity(true);
    let mapped = fs::InodeMetadata {
        kind: fs::InodeType::File,
        mode: 0,
        uid: 1000,
        gid: 1001,
    };
    let global = fs::InodeMetadata {
        uid: 0,
        gid: 0,
        ..mapped
    };
    assert!(identity.permits(mapped, 6));
    assert!(!identity.permits(global, 6));
    assert!(identity.owns_inode(1000));
    assert!(!identity.owns_inode(0));
    assert_eq!(
        identity.write_kill_mask(fs::InodeType::File, 0o4777, 1001),
        0o4000
    );
    let before = caller.credential_res_ids(true);
    assert_eq!(
        caller.set_credential_res_ids(true, [0, 1, u32::MAX]),
        Err(task::CredentialUpdateError::Invalid)
    );
    assert_eq!(caller.credential_res_ids(true), before);
    assert_eq!(caller.set_filesystem_id(true, 1), 0);
    assert_eq!(caller.observed_credentials().fsids[0], 1000);
    // Unmapped executable owners do not confer a set-ID transition into an ancestor identity.
    let aux = caller.exec_facts(0o6755, 0, 0);
    assert_eq!((aux.uid, aux.euid, aux.gid, aux.egid), (0, 0, 0, 0));
    assert_eq!(caller.observed_credentials().uids, [1000; 3]);
}

#[test]
fn actual_uts_unshare_copies_current_names_without_affecting_existing_membership() {
    let caller = task::TaskControlBlock::root();
    let sibling = caller.sibling();
    let original = caller.uts_namespace();
    original
        .set_name(true, b"before-copy", &caller.namespace_authority())
        .unwrap();
    caller
        .unshare_namespaces(
            false, false, false, true, false, false, false, false, false, false,
        )
        .unwrap();
    assert!(Arc::ptr_eq(&original, &sibling.uts_namespace()));
    assert!(!Arc::ptr_eq(&original, &caller.uts_namespace()));
    assert_eq!(
        caller.uts_namespace().snapshot().hostname,
        original.snapshot().hostname
    );
    caller
        .uts_namespace()
        .set_name(true, b"after-copy", &caller.namespace_authority())
        .unwrap();
    assert_eq!(&original.snapshot().hostname[..12], b"before-copy\0");
    let weak = Arc::downgrade(&caller.uts_namespace());
    caller.retire_namespaces_for_test();
    assert!(weak.upgrade().is_none());
}

#[test]
fn nondumpable_proc_access_requires_the_mm_namespace_even_after_user_unshare() {
    let caller = task::TaskControlBlock::root_with_pid(41);
    let target = task::TaskControlBlock::root_with_pid(42);
    let parent = caller.namespace_authority();
    let child = namespace::UserNamespace::child(
        &parent,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    child
        .publish_map(true, b"0 0 1\n", &parent, &parent)
        .unwrap();
    child
        .publish_map(false, b"0 0 1\n", &parent, &parent)
        .unwrap();
    caller
        .set_namespace(namespace::NamespaceReference::new(child.clone()))
        .unwrap();
    target
        .set_namespace(namespace::NamespaceReference::new(child))
        .unwrap();
    assert!(caller.proc_thread_permission(&target));
    target.set_user_dumpable(false);
    // Current credentials grant child CAP_SYS_PTRACE, but both original MMs belong to initial.
    assert!(!caller.proc_thread_permission(&target));
    assert!(!caller.proc_permission(&target));
    // exec creates a new mm owned by the current user namespace, without changing credentials.
    target.exec_setid(0o755, 0, 0);
    target.set_user_dumpable(false);
    assert!(caller.proc_thread_permission(&target));
    assert!(caller.proc_permission(&target));
}

#[test]
fn scoped_inode_operations_use_euid_ancestry_and_cannot_borrow_changed_fsuid() {
    use syscall_abi::capability::Capability;
    let initial = crate::namespace::boot();
    crate::ipc::initialize_namespaces().unwrap();
    let owner = crate::namespace::NamespaceAuthority {
        namespace: initial.clone(),
        effective_uid: 1000,
        effective_gid: 1000,
        effective_capabilities: 0,
    };
    let child = crate::namespace::UserNamespace::child(
        &owner,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let forged_fsuid = fs::AccessIdentity::new(1000, 1000, None, initial.clone(), 0, 2000);
    assert!(!forged_fsuid.namespace_capable(&child, Capability::SYS_RESOURCE));
    let actual_owner = fs::AccessIdentity::new(2000, 1000, None, initial, 0, 1000);
    assert!(actual_owner.namespace_capable(&child, Capability::SYS_RESOURCE));
}

#[test]
fn actual_proc_inode_owner_uses_dumpability_and_exact_world_directory_policy() {
    let target = task::TaskControlBlock::root();
    target.set_credential_id(false, 1001).unwrap();
    target.set_credential_id(true, 1000).unwrap();
    assert!(!target.user_dumpable());
    assert_eq!(target.observed_inode_owner(0o040555), (1000, 1001));
    assert_eq!(target.observed_inode_owner(0o100644), (0, 0));
    assert_eq!(target.observed_inode_owner(0o040550), (0, 0));
    target.set_user_dumpable(true);
    assert_eq!(target.observed_inode_owner(0o100644), (1000, 1001));
    assert_eq!(target.set_filesystem_id(true, 123), 1000);
    assert_eq!(target.observed_inode_owner(0o100644), (1000, 1001));
    let privileged = task::TaskControlBlock::root();
    privileged.set_user_dumpable(true);
    assert_eq!(privileged.set_filesystem_id(true, 123), 0);
    assert!(!privileged.user_dumpable());
    assert_eq!(privileged.observed_inode_owner(0o100644), (0, 0));
}
