#![feature(allocator_ext)]
#![allow(dead_code)]
//! Production tmpfs metadata helper, permission policy and TaskMutex.
//! Other inode metadata fields and MemoryFile pages are seams; actual pages/resize run in guest.
extern crate alloc;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
thread_local! { static FAIL_NEXT:Cell<bool>=const {Cell::new(false)}; }
struct FaultAllocator;
// SAFETY: one thread-local fault; every successful allocation/deallocation delegates to System.
unsafe impl GlobalAlloc for FaultAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if FAIL_NEXT
            .try_with(|flag| flag.replace(false))
            .unwrap_or(false)
        {
            core::ptr::null_mut()
        } else {
            unsafe { System.alloc(layout) }
        }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }
}
#[global_allocator]
static ALLOCATOR: FaultAllocator = FaultAllocator;

#[path = "../src/interruptible_event_target.rs"]
mod target;
#[path = "../../../kernel/src/fs"]
mod fs {
    use crate::execution::sync::{TaskMutex, TaskMutexGuard};
    use alloc::sync::Arc;
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum FileSystemError {
        AccessDenied,
        InvalidOperation,
        PermissionDenied,
        OutOfMemory,
        IsDirectory,
        NotFound,
        AlreadyExists,
        NameTooLong,
        NotSupported,
        IoError,
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum InodeType {
        CharacterDevice,
        File,
        Directory,
        Socket,
    }
    #[derive(Clone, Copy)]
    pub(crate) struct InodeMetadata {
        pub(crate) kind: InodeType,
        pub(crate) mode: u32,
        pub(crate) uid: u32,
        pub(crate) gid: u32,
    }
    // 仅metadata/backend边界；权限算法仍加载生产AccessIdentity/xattr policy。
    pub(crate) trait Inode {
        fn metadata(&self) -> Result<InodeMetadata, FileSystemError>;
        fn check_xattr_write(&self) -> Result<(), FileSystemError> {
            Ok(())
        }
        fn require_access(
            &self,
            identity: &AccessIdentity,
            requested: u8,
        ) -> Result<(), FileSystemError> {
            identity.require(self.metadata()?, requested)
        }
    }
    mod inode_times;
    mod permission;
    mod xattr;
    use permission::OwnerModeState;
    pub(crate) use permission::{AccessIdentity, OwnerModeChange};
    pub(crate) struct MemoryFile {
        sequence: TaskMutex<()>,
    }
    impl MemoryFile {
        fn begin_write(&self) -> Result<TaskMutexGuard<'_, ()>, FileSystemError> {
            self.sequence
                .lock()
                .map_err(|_| FileSystemError::OutOfMemory)
        }
    }
    enum Body {
        File(Arc<MemoryFile>),
        Directory,
        Symlink(()),
        Special(()),
    }
    struct State {
        xattrs: xattr::Xattrs,
        mode: u16,
        uid: u32,
        gid: u32,
        times: inode_times::TimeOwner,
    }
    fn now_seconds() -> u64 {
        1
    }
    struct TmpInode {
        body: Body,
        state: TaskMutex<State>,
    }
    impl TmpInode {
        fn state(&self) -> Result<TaskMutexGuard<'_, State>, FileSystemError> {
            self.state.lock().map_err(|_| FileSystemError::OutOfMemory)
        }
        fn kind(&self) -> InodeType {
            InodeType::File
        }
    }
    #[path = "tmpfs/content_mutation.rs"]
    mod content_mutation;
    fn inode() -> Arc<TmpInode> {
        crate::namespace::boot();
        Arc::new(TmpInode {
            body: Body::File(Arc::new(MemoryFile {
                sequence: TaskMutex::new(()),
            })),
            state: TaskMutex::new(State {
                xattrs: xattr::Xattrs::new(),
                mode: 0o6777,
                uid: 2000,
                gid: 2000,
                times: inode_times::TimeOwner::new(),
            }),
        })
    }
    #[test]
    fn published_allocation_completes_contended_metadata_without_new_allocation() {
        let inode = inode();
        let held = inode.state().unwrap();
        let worker_inode = inode.clone();
        let target = crate::target::Target::new();
        let worker_target = target.clone();
        let worker = std::thread::spawn(move || {
            let _binding = worker_target.bind();
            let mut wait = crate::execution::sync::TaskMutexWaitPreparation::prepare().unwrap();
            let identity = AccessIdentity::new(1000, 1000, None, crate::namespace::boot(), 0, 1000);
            crate::FAIL_NEXT.set(true);
            worker_inode.remove_write_privileges_prepared(Some(&identity), &mut wait);
            let no_alloc = crate::FAIL_NEXT.replace(false);
            (no_alloc, worker_inode.state().unwrap().mode)
        });
        target.wait_until_sleeping();
        drop(held);
        assert_eq!(worker.join().unwrap(), (true, 0o0777));
        assert_eq!(inode.state().unwrap().times.snapshot().ctime, 1);
    }
    #[test]
    fn fsetid_preserves_mode_but_never_preserves_nonempty_file_capabilities() {
        let inode = inode();
        {
            let mut state = inode.state().unwrap();
            state
                .xattrs
                .change(syscall_abi::file_capability::NAME, Some(vec![1; 24]), 0)
                .unwrap();
            state
                .xattrs
                .change(b"user.keep", Some(b"content".to_vec()), 0)
                .unwrap();
        }
        crate::FAIL_NEXT.set(true);
        inode
            .remove_write_privileges(Some(&AccessIdentity::root()))
            .unwrap();
        assert!(crate::FAIL_NEXT.replace(false));
        let state = inode.state().unwrap();
        assert_eq!(state.mode, 0o6777);
        assert_eq!(
            state.xattrs.get(syscall_abi::file_capability::NAME),
            Err(FileSystemError::NotFound)
        );
        assert_eq!(state.xattrs.get(b"user.keep").unwrap(), b"content");
    }

    #[test]
    fn noop_chown_kills_caps_while_chmod_and_empty_capability_attribute_keep_their_values() {
        let inode = inode();
        inode
            .state()
            .unwrap()
            .xattrs
            .change(syscall_abi::file_capability::NAME, Some(vec![1; 24]), 0)
            .unwrap();
        inode
            .update_owner_mode(OwnerModeChange::chmod(AccessIdentity::root(), 0o755))
            .unwrap();
        assert_eq!(
            inode
                .state()
                .unwrap()
                .xattrs
                .get(syscall_abi::file_capability::NAME)
                .unwrap()
                .len(),
            24
        );
        inode
            .update_owner_mode(OwnerModeChange::chown(AccessIdentity::root(), None, None))
            .unwrap();
        assert_eq!(
            inode
                .state()
                .unwrap()
                .xattrs
                .get(syscall_abi::file_capability::NAME),
            Err(FileSystemError::NotFound)
        );
        inode
            .state()
            .unwrap()
            .xattrs
            .change(syscall_abi::file_capability::NAME, Some(Vec::new()), 0)
            .unwrap();
        inode
            .remove_write_privileges(Some(&AccessIdentity::root()))
            .unwrap();
        assert_eq!(
            inode
                .state()
                .unwrap()
                .xattrs
                .get(syscall_abi::file_capability::NAME)
                .unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn xattr_flags_both_bits_and_insertion_oom_preserve_the_actual_previous_table() {
        let mut attributes = xattr::Xattrs::new();
        assert_eq!(
            attributes.change(b"user.value", Some(Vec::new()), 3),
            Err(FileSystemError::NotFound)
        );
        attributes
            .change(b"user.value", Some(b"old".to_vec()), 1)
            .unwrap();
        assert_eq!(
            attributes.change(b"user.value", Some(b"new".to_vec()), 3),
            Err(FileSystemError::AlreadyExists)
        );
        let value = b"another".to_vec();
        crate::FAIL_NEXT.set(true);
        assert_eq!(
            attributes.change(b"user.second", Some(value), 0),
            Err(FileSystemError::OutOfMemory)
        );
        assert!(!crate::FAIL_NEXT.replace(false));
        assert_eq!(attributes.get(b"user.value").unwrap(), b"old");
        assert_eq!(
            attributes.get(b"user.second"),
            Err(FileSystemError::NotFound)
        );
        assert_eq!(attributes.list(|_| true).unwrap(), b"user.value\0");
    }

    #[test]
    fn chmod_uses_the_same_sequence_as_content_permission_commit() {
        let inode = inode();
        let write = inode.file().unwrap().begin_write().unwrap();
        let worker_inode = inode.clone();
        let target = crate::target::Target::new();
        let worker_target = target.clone();
        let worker = std::thread::spawn(move || {
            let _binding = worker_target.bind();
            worker_inode
                .update_owner_mode(OwnerModeChange::chmod(AccessIdentity::root(), 0o4755))
                .unwrap();
        });
        target.wait_until_sleeping();
        inode
            .remove_write_privileges(Some(&AccessIdentity::new(
                1000,
                1000,
                None,
                crate::namespace::boot(),
                0,
                1000,
            )))
            .unwrap();
        assert_eq!(inode.state().unwrap().mode, 0o0777);
        drop(write);
        worker.join().unwrap();
        assert_eq!(inode.state().unwrap().mode, 0o4755);
    }

    fn capability_value(magic: u32, root_uid: u32) -> Vec<u8> {
        let (bytes, length) = syscall_abi::file_capability::FileCapabilityData {
            magic,
            permitted: 1 << 10,
            inheritable: 0,
            root_uid,
        }
        .encode()
        .unwrap();
        bytes[..length].to_vec()
    }
    fn mapped_user(uid: u32, gid: u32) -> Arc<crate::namespace::UserNamespace> {
        let root = crate::namespace::NamespaceAuthority {
            namespace: crate::namespace::boot(),
            effective_uid: 0,
            effective_gid: 0,
            effective_capabilities: syscall_abi::capability::VALID_MASK,
        };
        let child = crate::namespace::UserNamespace::child(
            &root,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX,
            },
        )
        .unwrap();
        child
            .publish_map(true, format!("0 {uid} 1\n").as_bytes(), &root, &root)
            .unwrap();
        child
            .publish_map(false, format!("0 {gid} 1\n").as_bytes(), &root, &root)
            .unwrap();
        child
    }

    #[test]
    fn actual_user_namespace_setcap_rootid_conversion_and_getsecurity_projection() {
        use syscall_abi::file_capability::{
            EFFECTIVE, FileCapabilityData, NAME, REVISION_2, REVISION_3,
        };
        let user = mapped_user(1000, 1001);
        let caller = AccessIdentity::new(
            1000,
            1001,
            None,
            user.clone(),
            syscall_abi::capability::VALID_MASK,
            1000,
        );
        let owner = OwnerModeState::new(InodeType::File, 0o0000, 1000, 1001);
        let mut change = xattr::XattrChange::new(
            NAME,
            Some(capability_value(REVISION_2 | EFFECTIVE, 0)),
            0,
            &caller,
            crate::namespace::boot(),
        );
        change.prepare(&owner, Ok(())).unwrap();
        let stored = change.value.unwrap();
        let decoded = FileCapabilityData::from_disk(&stored).unwrap();
        assert_eq!(decoded.magic, REVISION_3 | EFFECTIVE);
        assert_eq!(decoded.root_uid, 1000);
        let local =
            xattr::project_capability(stored.clone(), &crate::namespace::boot(), &user).unwrap();
        assert_eq!(
            FileCapabilityData::from_disk(&local).unwrap().magic,
            REVISION_2 | EFFECTIVE
        );
        let global = xattr::project_capability(
            stored.clone(),
            &crate::namespace::boot(),
            &crate::namespace::boot(),
        )
        .unwrap();
        assert_eq!(
            FileCapabilityData::from_disk(&global).unwrap().root_uid,
            1000
        );
        assert!(matches!(
            xattr::project_capability(stored, &crate::namespace::boot(), &mapped_user(2000, 2001)),
            Err(xattr::XattrError::Overflow)
        ));
        // 当前user创建的SB接收v2时保持原wire，rootid由真实SB namespace定义。
        let value = capability_value(REVISION_2 | EFFECTIVE, 0);
        let mut change =
            xattr::XattrChange::new(NAME, Some(value.clone()), 0, &caller, user.clone());
        change.prepare(&owner, Ok(())).unwrap();
        assert_eq!(change.value, Some(value));
        let visible =
            xattr::project_capability(change.value.unwrap(), &user, &crate::namespace::boot())
                .unwrap();
        assert_eq!(
            FileCapabilityData::from_disk(&visible).unwrap().root_uid,
            1000
        );
    }

    #[test]
    fn xattr_authority_uses_live_inode_facts_and_security_namespace_rules() {
        use syscall_abi::{
            capability::Capability,
            file_capability::{EFFECTIVE, NAME, REVISION_2},
        };
        let initial = crate::namespace::boot();
        let caller = AccessIdentity::new(1000, 1000, None, initial.clone(), 0, 1000);
        let owner = OwnerModeState::new(InodeType::File, 0o600, 1000, 1000);
        let mut change =
            xattr::XattrChange::new(b"user.value", Some(vec![1]), 0, &caller, initial.clone());
        change.prepare(&owner, Ok(())).unwrap();
        let moved = OwnerModeState::new(InodeType::File, 0o600, 2000, 2000);
        // 同一个request必须使用mutation gate下实时owner；不保存早先授权结果。
        assert_eq!(
            change.prepare(&moved, Ok(())),
            Err(FileSystemError::AccessDenied)
        );
        let mut cap = xattr::XattrChange::new(
            NAME,
            Some(capability_value(REVISION_2 | EFFECTIVE, 0)),
            0,
            &caller,
            initial.clone(),
        );
        assert_eq!(
            cap.prepare(&owner, Ok(())),
            Err(FileSystemError::PermissionDenied)
        );
        let mut empty =
            xattr::XattrChange::new(NAME, Some(Vec::new()), 0, &caller, initial.clone());
        empty.prepare(&owner, Ok(())).unwrap();
        let mut remove = xattr::XattrChange::new(NAME, None, 2, &caller, initial.clone());
        assert_eq!(
            remove.prepare(&owner, Ok(())),
            Err(FileSystemError::PermissionDenied)
        );
        let user = mapped_user(1000, 1000);
        let nested = AccessIdentity::new(
            1000,
            1000,
            None,
            user.clone(),
            Capability::SYS_ADMIN.mask(),
            1000,
        );
        let mut trusted =
            xattr::XattrChange::new(b"trusted.private", Some(vec![1]), 0, &nested, user.clone());
        assert_eq!(
            trusted.prepare(&owner, Ok(())),
            Err(FileSystemError::PermissionDenied)
        );
        let mut security =
            xattr::XattrChange::new(b"security.private", Some(vec![1]), 0, &nested, user.clone());
        security.prepare(&owner, Ok(())).unwrap();
        let mut global =
            xattr::XattrChange::new(b"security.private", Some(vec![1]), 0, &nested, initial);
        assert_eq!(
            global.prepare(&owner, Ok(())),
            Err(FileSystemError::PermissionDenied)
        );
    }
}

#[path = "."]
pub(crate) mod execution {
    pub(crate) mod timer {
        pub(crate) fn get_realtime_ns() -> u64 {
            1_000_000_000
        }
    }
    #[path = "../../../kernel/src/execution/sync"]
    pub(crate) mod sync {
        mod wait_completion;
        pub(crate) use wait_completion::WaitCompletion;
        mod task_wait;
        pub(crate) use task_wait::{
            TaskWaitKey, TaskWaitTarget, WaitResult, install_wait_target_factory,
        };
        #[allow(
            unused_imports,
            reason = "focused fixture does not consume every TaskMutex lock mode"
        )]
        mod task_mutex;
        pub(crate) use task_mutex::{TaskMutex, TaskMutexGuard, TaskMutexWaitPreparation};
    }
}
