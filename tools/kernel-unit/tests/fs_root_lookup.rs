#![feature(allocator_ext)]
#![allow(dead_code)]
//! Actual VFS traversal, opened-entry chain and exact lifecycle index. Inode storage,
//! mount topology and credential checks are explicit adapters; no copied path algorithm.
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
mod timer {
    pub(crate) fn get_realtime_ns() -> u64 {
        1_000_000_000
    }
}
#[macro_export]
macro_rules! warn { ($($arg:tt)*) => { eprintln!($($arg)*) }; }
// 显式 scheduler notification adapter；event/seen-event/topology 均加载生产 owner。
mod ipc {
    use alloc::sync::Arc;
    pub(crate) struct Pipe;
    pub(crate) struct PipeEnd(Arc<Pipe>);
    impl Pipe {
        pub(crate) fn notification_pair() -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), ()> {
            let pipe = Arc::try_new(Self).map_err(|_| ())?;
            Ok((
                Arc::try_new(PipeEnd(pipe.clone())).map_err(|_| ())?,
                Arc::try_new(PipeEnd(pipe)).map_err(|_| ())?,
            ))
        }
    }
    impl PipeEnd {
        pub(crate) fn pipe(&self) -> Arc<Pipe> {
            self.0.clone()
        }
        pub(crate) fn signal_readiness(&self) {}
        pub(crate) fn drain_readiness(&self) -> u64 {
            0
        }
    }
}
// Blocking mutex implementation is a host scheduler adapter; actual namespace guard,
// traversal/admission/projection ownership is loaded from production.
mod sync {
    pub(crate) struct TaskEvent;
    impl TaskEvent {
        pub(crate) const fn new() -> Self {
            Self
        }
        pub(crate) fn signal(&self) {}
        pub(crate) fn wait_idle(&self) {
            unreachable!("worker scheduler adapter not executed in host fixture");
        }
    }
    pub(crate) struct TaskMutexWaitPreparation;
    impl TaskMutexWaitPreparation {
        pub(crate) fn prepare() -> Result<Self, ()> {
            Ok(Self)
        }
    }

    pub(crate) fn next_readiness_generation() -> u64 {
        static NEXT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
        NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
    }
    thread_local! {
        static BLOCKED: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const { std::cell::RefCell::new(None) };
    }
    pub(crate) fn observe_blocking(sender: std::sync::mpsc::Sender<()>) {
        BLOCKED.with(|hook| *hook.borrow_mut() = Some(sender));
    }
    pub(crate) struct TaskMutex<T>(std::sync::Mutex<T>);
    pub(crate) type TaskMutexGuard<'a, T> = std::sync::MutexGuard<'a, T>;
    impl<T> TaskMutex<T> {
        pub(crate) const fn new(value: T) -> Self {
            Self(std::sync::Mutex::new(value))
        }
        pub(crate) fn lock_prepared(
            &self,
            _: &mut TaskMutexWaitPreparation,
        ) -> TaskMutexGuard<'_, T> {
            self.lock().unwrap()
        }
        pub(crate) fn try_lock(&self) -> Option<TaskMutexGuard<'_, T>> {
            self.0.try_lock().ok()
        }
        pub(crate) fn lock(&self) -> Result<TaskMutexGuard<'_, T>, ()> {
            match self.0.try_lock() {
                Ok(guard) => Ok(guard),
                Err(std::sync::TryLockError::Poisoned(_)) => Err(()),
                Err(std::sync::TryLockError::WouldBlock) => {
                    BLOCKED.with(|hook| {
                        if let Some(sender) = hook.borrow_mut().take() {
                            sender.send(()).unwrap();
                        }
                    });
                    self.0.lock().map_err(|_| ())
                }
            }
        }
    }
}
#[path = "../../../kernel/src/fs"]
mod fs {
    // notification 子系统边界只记录 actual last-dput 的调用事实，不复制判断 links/投递算法。
    static RECLAIMED_DENTRY_COUNT: core::sync::atomic::AtomicU64 =
        core::sync::atomic::AtomicU64::new(0);
    static RECLAIMED_DENTRY_INODE: core::sync::atomic::AtomicU64 =
        core::sync::atomic::AtomicU64::new(0);
    pub(crate) fn notify_dentry_reclaimed(inode: &dyn Inode) {
        RECLAIMED_DENTRY_INODE.store(
            inode.metadata().unwrap().inode,
            core::sync::atomic::Ordering::Relaxed,
        );
        RECLAIMED_DENTRY_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }

    static SEARCH_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    // 此 fixture 验证实际 dentry/path 拓扑；pseudo factory 在 namespace_files 加载实际 owner。
    mod anonymous {
        pub(in crate::fs) struct PseudoFileSystem;
        impl PseudoFileSystem {
            pub(in crate::fs) fn open(
                self,
                _: alloc::sync::Arc<super::vfs::Dentry>,
            ) -> Result<alloc::sync::Arc<super::OpenedFile>, super::FileSystemError> {
                unreachable!("pseudo factory belongs to namespace_files fixture")
            }
        }
    }
    use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use spin::Mutex;
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum FileSystemError {
        NoDevice,
        NotFound,
        InvalidPath,
        NameTooLong,
        InvalidFileSystem,
        InvalidOperation,
        OutOfMemory,
        NotDirectory,
        IsDirectory,
        Loop,
        IoError,
        Busy,
        Retry,
        TextBusy,
        ReadOnly,
        PermissionDenied,
        AlreadyExists,
        NoSpace,
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum InodeType {
        File,
        Directory,
        SymLink,
    }
    pub(crate) struct Metadata {
        filesystem: u64,
        inode: u64,
    }
    pub(crate) enum LinkTarget {
        Text(Vec<u8>),
        Opened(Arc<OpenedFile>),
    }
    mod write_access;
    pub(crate) use write_access::{InodeAccessLease, InodeWriteAccess};
    pub(crate) trait Inode: Send + Sync {
        fn namespace_reference(&self) -> Option<crate::namespace::NamespaceReference> {
            None
        }
        fn write_access(&self) -> Option<&InodeWriteAccess> {
            None
        }
        fn touch_access(&self, _now: u64, _relatime: bool) -> Result<(), FileSystemError> {
            Ok(())
        }
        fn inode_type(&self) -> InodeType;
        fn metadata(&self) -> Result<Metadata, FileSystemError>;
        fn require_access(
            &self,
            identity: &AccessIdentity,
            requested: u8,
        ) -> Result<(), FileSystemError> {
            if requested == 1 {
                SEARCH_CALLS.fetch_add(1, Ordering::Relaxed);
            }
            identity.require(self.metadata()?, requested)
        }
        fn filesystem_id(&self) -> usize;
        fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError>;
        fn link_target(&self) -> Result<LinkTarget, FileSystemError> {
            self.read_link().map(LinkTarget::Text)
        }
        fn sync_storage(&self) -> Result<(), FileSystemError> {
            Ok(())
        }
        fn read_link(&self) -> Result<Vec<u8>, FileSystemError> {
            Err(FileSystemError::InvalidPath)
        }
    }
    type SpawnKernelThread =
        fn(&'static str, alloc::boxed::Box<dyn FnOnce() + Send>) -> Result<(), ()>;
    pub(crate) struct KernelThreadSupport {
        pub(crate) spawn: SpawnKernelThread,
    }
    pub(crate) struct FileSystemStatistics {
        pub(crate) type_name: &'static str,
        pub(crate) flags: u64,
    }
    pub(crate) trait FileSystem: Send + Sync {
        fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
            Ok(FileSystemStatistics {
                type_name: "fixture",
                flags: 0,
            })
        }
        fn show_mount_options(&self) -> Result<Vec<u8>, FileSystemError> {
            Ok(Vec::new())
        }
        fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError>;
        fn shutdown(&self) -> Result<(), FileSystemError> {
            Ok(())
        }
        fn remount(
            &self,
            _: &[u8],
            _: &crate::namespace::UserNamespace,
            _: bool,
        ) -> Result<(), FileSystemError> {
            Ok(())
        }
    }
    pub(crate) struct BlockNode;
    impl BlockNode {
        fn begin_mount(&self) -> Result<(), FileSystemError> {
            Ok(())
        }
        fn end_mount(&self) {}
        fn number(&self) -> DeviceNumber {
            DeviceNumber::new(0, 0)
        }
    }
    #[path = "../../../kernel/src/fs/devnum.rs"]
    mod devnum;
    pub(crate) use devnum::DeviceNumber;
    mod devnode {
        use super::DeviceNumber;
        pub(crate) fn block_node(_: DeviceNumber) -> Option<alloc::sync::Arc<super::BlockNode>> {
            None
        }
    }
    pub(crate) fn filesystem_unmounted(_: usize) {}
    pub(crate) struct AccessIdentity;
    impl AccessIdentity {
        fn namespace_capable(
            &self,
            _: &crate::namespace::UserNamespace,
            _: syscall_abi::capability::Capability,
        ) -> bool {
            true
        }
        fn root() -> Self {
            Self
        }
        fn require(&self, _: Metadata, _: u8) -> Result<(), FileSystemError> {
            Ok(())
        }
    }
    mod block_node {
        pub(super) fn wrap_block_node(
            inode: alloc::sync::Arc<dyn super::Inode>,
        ) -> Result<alloc::sync::Arc<dyn super::Inode>, super::FileSystemError> {
            Ok(inode)
        }
    }
    mod page_cache {
        pub(super) fn sync_all() -> Result<(), super::FileSystemError> {
            Ok(())
        }
        pub(super) fn evict_filesystem(_: usize) -> Result<(), super::FileSystemError> {
            Ok(())
        }
        pub(super) fn evict_if_unlinked(_: &alloc::sync::Arc<dyn super::Inode>) {}
    }
    // nsfs具体adapter在namespace_files加载；本fixture只替换取得adapter的边界，
    // mount writer/index/retirement全用生产MountedFilesystem，无dummy拓扑或命名空间pin。
    mod nsfs {
        use super::*;
        static FILESYSTEM: spin::Once<Arc<vfs::MountedFilesystem>> = spin::Once::new();
        struct Root;
        impl Inode for Root {
            fn inode_type(&self) -> InodeType {
                InodeType::Directory
            }
            fn metadata(&self) -> Result<Metadata, FileSystemError> {
                Ok(Metadata {
                    filesystem: 11,
                    inode: 1,
                })
            }
            fn filesystem_id(&self) -> usize {
                11
            }
            fn find_child(&self, _: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
                Err(FileSystemError::NotFound)
            }
        }
        struct Adapter;
        impl FileSystem for Adapter {
            fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
                Ok(Arc::try_new(Root).map_err(|_| FileSystemError::OutOfMemory)?)
            }
            fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
                Ok(FileSystemStatistics {
                    type_name: "nsfs",
                    flags: 0x20,
                })
            }
        }
        pub(super) fn mounted_filesystem() -> Result<Arc<vfs::MountedFilesystem>, FileSystemError> {
            FILESYSTEM
                .try_call_once(|| {
                    let adapter =
                        Arc::try_new(Adapter).map_err(|_| FileSystemError::OutOfMemory)?;
                    vfs::MountedFilesystem::new(
                        adapter,
                        None,
                        false,
                        crate::namespace::initial_user(),
                    )
                    .map(|(owner, _)| owner)
                })
                .cloned()
        }
    }
    mod vfs {
        use super::*;
        mod dentry;
        mod dentry_index;
        mod lookup_origin;
        #[path = "mount_attachment.rs"]
        mod mount_attachment;
        mod mount_copy;
        mod mount_flags;
        mod mount_index;
        mod mount_limit;
        use mount_limit::mount_limit;
        mod mount_namespace;
        mod mount_operations;
        mod mount_propagation;
        mod mount_publication;
        mod mount_retirement;
        mod mount_table;
        mod mount_unmount;
        mod mount_view;
        mod mounted_filesystem;
        mod namespace_access;
        mod opened;
        mod path_lookup;
        pub(in crate::fs) use dentry::Dentry;
        use dentry_index::DentryIndex;
        pub(crate) use lookup_origin::LookupOrigin;
        use mount_flags::MountFlags;
        use mount_namespace::{MountNamespace, UnmountMode};
        use mount_propagation::MountPropagation;
        use mount_view::{Attachment, MountRestrictions, MountScope, MountView, WriteOpen};
        pub(in crate::fs) use mounted_filesystem::MountedFilesystem;
        use namespace_access::NamespaceAccess;
        pub(crate) use opened::OpenedFile;
        pub(crate) struct VirtualFileSystem {
            root: Mutex<Option<Arc<OpenedFile>>>,
            namespace: Mutex<Option<Arc<MountNamespace>>>,
            dentries: DentryIndex,
            namespace_mutation: crate::sync::TaskMutex<()>,
        }
        static VFS: VirtualFileSystem = VirtualFileSystem {
            root: Mutex::new(None),
            namespace: Mutex::new(None),
            dentries: DentryIndex::new(),
            namespace_mutation: crate::sync::TaskMutex::new(()),
        };
        pub(super) fn vfs() -> &'static VirtualFileSystem {
            &VFS
        }
        impl VirtualFileSystem {
            fn root_opened(&self) -> Result<Arc<OpenedFile>, FileSystemError> {
                self.root.lock().clone().ok_or(FileSystemError::NotFound)
            }
        }
        static CASE: Mutex<()> = Mutex::new(());
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        struct Node {
            id: u64,
            kind: InodeType,
            filesystem: AtomicU64,
            children: Mutex<BTreeMap<Vec<u8>, Arc<Node>>>,
            target: Vec<u8>,
            magic: Mutex<Option<Arc<OpenedFile>>>,
            namespace: Option<crate::namespace::NamespaceReference>,
            fail_metadata: AtomicBool,
            fail_target: AtomicBool,
            find_pause: Mutex<Option<Arc<(std::sync::Barrier, std::sync::Barrier)>>>,
        }
        impl Node {
            fn new(kind: InodeType, target: &[u8]) -> Arc<Self> {
                Arc::new(Self {
                    id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                    filesystem: AtomicU64::new(0),
                    kind,
                    children: Mutex::new(BTreeMap::new()),
                    target: target.to_vec(),
                    magic: Mutex::new(None),
                    namespace: None,
                    fail_metadata: AtomicBool::new(false),
                    fail_target: AtomicBool::new(false),
                    find_pause: Mutex::new(None),
                })
            }
            fn add(&self, name: &[u8], child: Arc<Self>) {
                // 每个 fixture superblock 用根 inode 的独立 identity；子 inode 继承实际所属卷。
                child.attach_filesystem(self.filesystem_id());
                self.children.lock().insert(name.to_vec(), child);
            }
            fn attach_filesystem(&self, filesystem: usize) {
                self.filesystem.store(filesystem as u64, Ordering::Relaxed);
                for child in self.children.lock().values() {
                    child.attach_filesystem(filesystem);
                }
            }
            fn namespace_file(reference: crate::namespace::NamespaceReference) -> Arc<Self> {
                let mut inode = Self::new(InodeType::File, b"");
                Arc::get_mut(&mut inode).unwrap().namespace = Some(reference);
                inode
            }
        }
        impl Inode for Node {
            fn namespace_reference(&self) -> Option<crate::namespace::NamespaceReference> {
                self.namespace.clone()
            }
            fn inode_type(&self) -> InodeType {
                self.kind
            }
            fn metadata(&self) -> Result<Metadata, FileSystemError> {
                if self.fail_metadata.load(Ordering::Relaxed) {
                    Err(FileSystemError::IoError)
                } else {
                    Ok(Metadata {
                        filesystem: self.filesystem_id() as u64,
                        inode: self.id,
                    })
                }
            }
            fn filesystem_id(&self) -> usize {
                if self.namespace.is_some() {
                    11
                } else {
                    let filesystem = self.filesystem.load(Ordering::Relaxed);
                    if filesystem == 0 {
                        self.id as usize + 100
                    } else {
                        filesystem as usize
                    }
                }
            }
            fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
                if self.kind != InodeType::Directory {
                    return Err(FileSystemError::NotDirectory);
                }
                let pause = self.find_pause.lock().clone();
                if let Some(pause) = pause {
                    pause.0.wait();
                    pause.1.wait();
                }
                self.children
                    .lock()
                    .get(name)
                    .cloned()
                    .map(|inode| inode as Arc<dyn Inode>)
                    .ok_or(FileSystemError::NotFound)
            }
            fn read_link(&self) -> Result<Vec<u8>, FileSystemError> {
                Ok(self.target.clone())
            }
            fn link_target(&self) -> Result<LinkTarget, FileSystemError> {
                if self.fail_target.swap(false, Ordering::Relaxed) {
                    return Err(FileSystemError::OutOfMemory);
                }
                match self.magic.lock().clone() {
                    Some(opened) => Ok(LinkTarget::Opened(opened)),
                    None => self.read_link().map(LinkTarget::Text),
                }
            }
        }
        struct TestFilesystem(Arc<Node>);
        impl FileSystem for TestFilesystem {
            fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
                Ok(self.0.clone())
            }
            fn shutdown(&self) -> Result<(), FileSystemError> {
                // Actual filesystem final Drop 必须在 gate 外；重入 actual admission 会揭露锁内清理。
                let transaction = VFS.namespace()?;
                drop(transaction);
                Ok(())
            }
        }
        struct Fixture {
            global: Arc<OpenedFile>,
            jail: Arc<OpenedFile>,
            inner: Arc<OpenedFile>,
            file: Arc<Node>,
            jail_inode: Arc<Node>,
        }
        impl Fixture {
            fn new() -> Self {
                let previous_namespace = VFS.namespace.lock().take();
                let previous_root = VFS.root.lock().take();
                drop(previous_namespace);
                drop(previous_root);
                while mount_retirement::has_pending() {
                    let transaction = VFS.namespace().unwrap();
                    drop(transaction);
                }
                crate::namespace::boot();
                let root = Node::new(InodeType::Directory, b"");
                let jail = Node::new(InodeType::Directory, b"");
                let inner = Node::new(InodeType::Directory, b"");
                let file = Node::new(InodeType::File, b"");
                let outside = Node::new(InodeType::File, b"");
                root.add(b"jail", jail.clone());
                root.add(b"outside", outside);
                jail.add(b"inner", inner.clone());
                jail.add(b"file", file.clone());
                jail.add(b"absolute", Node::new(InodeType::SymLink, b"/file"));
                jail.add(b"relative", Node::new(InodeType::SymLink, b"inner/../file"));
                jail.add(b"escape", Node::new(InodeType::SymLink, b"/outside"));
                jail.add(b"loop", Node::new(InodeType::SymLink, b"loop"));
                let filesystem = MountedFilesystem::new(
                    Arc::new(TestFilesystem(root.clone())),
                    None,
                    false,
                    crate::namespace::initial_user(),
                )
                .unwrap()
                .0;
                let dentry = VFS
                    .dentries
                    .admit(Dentry::named(root, None, b"").unwrap())
                    .unwrap();
                let scope = MountScope::new(
                    crate::namespace::next_namespace_id(),
                    crate::namespace::initial_user(),
                )
                .unwrap();
                let view = MountView::new(
                    scope.clone(),
                    Arc::new(Vec::new()),
                    filesystem,
                    dentry.clone(),
                    None,
                    MountFlags::default(),
                    MountRestrictions::default(),
                )
                .unwrap();
                let namespace = MountNamespace::initial(scope).unwrap();
                let worker_membership = namespace.clone();
                assert!(matches!(
                    worker_membership.root(),
                    Err(FileSystemError::NotFound)
                ));
                namespace.install_root(view.clone()).unwrap();
                assert!(Arc::ptr_eq(&namespace, &worker_membership));
                assert!(
                    worker_membership
                        .root()
                        .unwrap()
                        .same_path(&OpenedFile::at(view.clone(), dentry.clone()).unwrap())
                );
                assert!(matches!(
                    namespace.install_root(view.clone()),
                    Err(FileSystemError::AlreadyExists)
                ));
                let global = OpenedFile::at(view, dentry).unwrap();
                *VFS.namespace.lock() = Some(namespace);
                let old = VFS.root.lock().replace(global.clone());
                drop(old);
                let jail_opened = VFS.open_file(b"/jail").unwrap();
                let inner_opened = VFS.open_file(b"/jail/inner").unwrap();
                Self {
                    global,
                    jail: jail_opened,
                    inner: inner_opened,
                    file,
                    jail_inode: jail,
                }
            }
            fn origin(&self) -> LookupOrigin {
                LookupOrigin::new(self.jail.clone(), self.inner.clone())
            }
        }
        #[test]
        fn actual_last_deleted_dentry_retires_notifications_after_path_pins_disappear() {
            let _case = CASE.lock();
            let f = Fixture::new();
            let opened = OpenedFile::child(f.file.clone(), f.jail.clone(), b"file").unwrap();
            let observer = opened.clone();
            let before = super::RECLAIMED_DENTRY_COUNT.load(Ordering::Relaxed);
            VFS.dentries.mark_unlinked(
                f.jail.inode_identity().unwrap(),
                b"file",
                opened.inode_identity().unwrap(),
            );
            drop(opened);
            assert_eq!(
                super::RECLAIMED_DENTRY_COUNT.load(Ordering::Relaxed),
                before
            );
            drop(observer);
            assert_eq!(
                super::RECLAIMED_DENTRY_COUNT.load(Ordering::Relaxed),
                before + 1
            );
            assert_eq!(
                super::RECLAIMED_DENTRY_INODE.load(Ordering::Relaxed),
                f.file.id
            );
        }
        #[test]
        fn anonymous_display_never_enters_namespace_index_or_label_lookup() {
            let _case = CASE.lock();
            let f = Fixture::new();
            let real = Node::new(InodeType::File, b"");
            let label = b"/memfd:collision (deleted)";
            f.jail_inode
                .add(&label[1..], Node::new(InodeType::File, b""));
            let opened = f
                .global
                .with_dentry(Dentry::anonymous(real.clone(), label.to_vec()).unwrap())
                .unwrap();
            assert!(opened.parent(&f.jail).unwrap().is_none());
            assert!(opened.dentry.index_key().unwrap().is_none());
            assert!(opened.dentry.snapshot().is_none());
            crate::allocation_failure::fail_next();
            let registered = VFS.dentries.admit(opened.dentry.clone()).unwrap();
            assert!(
                !crate::allocation_failure::was_consumed(),
                "anonymous admission must not allocate an index node"
            );
            assert!(Arc::ptr_eq(&registered, &opened.dentry));
            let link = Node::new(InodeType::SymLink, label);
            *link.magic.lock() = Some(opened.clone());
            f.jail_inode.add(b"anonymous", link);
            let reopened = VFS
                .open_file_at(f.origin(), b"/anonymous", &AccessIdentity)
                .unwrap();
            assert!(Arc::ptr_eq(&reopened, &opened));
            assert_eq!(reopened.inode_identity().unwrap().1, real.id);
            for path in [b"/anonymous/".as_slice(), b"/anonymous/child"] {
                assert!(matches!(
                    VFS.open_file_at(f.origin(), path, &AccessIdentity),
                    Err(FileSystemError::NotDirectory)
                ));
            }
            f.jail_inode.fail_metadata.store(true, Ordering::Relaxed);
            assert_eq!(opened.path(&f.jail, false).unwrap(), label);
        }
        #[test]
        fn anonymous_dentry_and_display_allocation_errors_preserve_identity() {
            let _case = CASE.lock();
            let f = Fixture::new();
            let real = Node::new(InodeType::File, b"");
            let label = b"pipe:[7]".to_vec();
            let before = Arc::strong_count(&real);
            crate::allocation_failure::fail_next();
            assert!(matches!(
                Dentry::anonymous(real.clone(), label),
                Err(FileSystemError::OutOfMemory)
            ));
            assert!(crate::allocation_failure::was_consumed());
            assert_eq!(Arc::strong_count(&real), before);
            let opened = f
                .global
                .with_dentry(Dentry::anonymous(real, b"pipe:[7]".to_vec()).unwrap())
                .unwrap();
            crate::allocation_failure::fail_next();
            assert!(matches!(
                opened.path(&f.jail, false),
                Err(FileSystemError::OutOfMemory)
            ));
            assert!(crate::allocation_failure::was_consumed());
            assert_eq!(opened.path(&f.jail, false).unwrap(), b"pipe:[7]");
        }
        #[test]
        fn create_walk_delivers_expanded_missing_leaf_without_restarting_original_path() {
            let _case = CASE.lock();
            let f = Fixture::new();
            f.jail_inode.add(
                b"dangling-relative",
                Node::new(InodeType::SymLink, b"inner/new-relative"),
            );
            f.jail_inode.add(
                b"dangling-absolute",
                Node::new(InodeType::SymLink, b"/inner/new-absolute"),
            );
            f.jail_inode.add(
                b"chain",
                Node::new(InodeType::SymLink, b"dangling-relative"),
            );
            let transaction = VFS.namespace().unwrap();
            for (link, expected) in [
                (b"/dangling-relative".as_slice(), b"new-relative".as_slice()),
                (b"/dangling-absolute", b"new-absolute"),
                (b"/chain", b"new-relative"),
            ] {
                match transaction
                    .resolve_for_create(f.origin(), link, false, &AccessIdentity)
                    .unwrap()
                {
                    path_lookup::PathLookup::Missing { parent, name } => {
                        assert!(parent.same_path(&f.inner));
                        assert_eq!(name, expected);
                    }
                    path_lookup::PathLookup::Existing(_) => {
                        panic!("dangling target unexpectedly exists")
                    }
                }
                assert!(matches!(
                    transaction.resolve_from(f.origin(), link, false, &AccessIdentity),
                    Err(FileSystemError::NotFound)
                ));
                assert!(
                    matches!(transaction.resolve_for_create(f.origin(), link, true, &AccessIdentity), Ok(path_lookup::PathLookup::Existing(opened)) if opened.inode().inode_type()==InodeType::SymLink)
                );
            }
            assert!(matches!(
                transaction.resolve_for_create(
                    f.origin(),
                    b"/missing-parent/child",
                    false,
                    &AccessIdentity
                ),
                Err(FileSystemError::NotFound)
            ));
            assert!(matches!(
                transaction.resolve_for_create(
                    f.origin(),
                    b"/missing-leaf/",
                    false,
                    &AccessIdentity
                ),
                Err(FileSystemError::IsDirectory)
            ));
            assert!(matches!(
                transaction.resolve_for_create(f.origin(), b"/inner/", false, &AccessIdentity),
                Err(FileSystemError::IsDirectory)
            ));
            assert!(matches!(
                transaction.resolve_for_create(f.origin(), b"/loop", false, &AccessIdentity),
                Err(FileSystemError::Loop)
            ));
        }
        #[test]
        fn root_only_walk_does_not_require_final_search_but_dot_consumes_a_component() {
            let _case = CASE.lock();
            let f = Fixture::new();
            let before = super::SEARCH_CALLS.load(Ordering::Relaxed);
            let root = VFS.open_file_at(f.origin(), b"/", &AccessIdentity).unwrap();
            assert!(root.same_path(&f.jail));
            assert_eq!(super::SEARCH_CALLS.load(Ordering::Relaxed), before);
            VFS.open_file_at(f.origin(), b".", &AccessIdentity).unwrap();
            assert_eq!(super::SEARCH_CALLS.load(Ordering::Relaxed), before + 1);
            assert!(matches!(
                VFS.open_file_at(f.origin(), b"", &AccessIdentity),
                Err(FileSystemError::NotFound)
            ));
        }
        #[test]
        fn absolute_symlinks_and_dotdot_use_explicit_root() {
            let _case = CASE.lock();
            let f = Fixture::new();
            for path in [
                b"/file".as_slice(),
                b"../../file",
                b"/absolute",
                b"/relative",
            ] {
                let entry = VFS.open_file_at(f.origin(), path, &AccessIdentity).unwrap();
                assert_eq!(entry.inode_identity().unwrap().1, f.file.id);
            }
            assert!(matches!(
                VFS.open_file_at(f.origin(), b"/escape", &AccessIdentity),
                Err(FileSystemError::NotFound)
            ));
            assert!(matches!(
                VFS.open_file_at(f.origin(), b"/loop", &AccessIdentity),
                Err(FileSystemError::Loop)
            ));
            assert_eq!(VFS.current_directory_path(f.origin()).unwrap(), b"/inner");
            assert_eq!(
                VFS.current_directory_path(LookupOrigin::new(f.jail.clone(), f.jail.clone()))
                    .unwrap(),
                b"/"
            );
        }
        #[test]
        fn dotdot_clamps_before_leaving_a_mounted_fs_root() {
            let _case = CASE.lock();
            let f = Fixture::new();
            let source = f.jail.clone();
            let namespace = VFS.namespace.lock().as_ref().unwrap().clone();
            let origin = source.mount.as_ref().unwrap();
            let view = MountView::new(
                namespace.scope.clone(),
                origin.source.clone(),
                origin.filesystem.clone(),
                source.dentry.clone(),
                Some(Attachment::new(origin, source.dentry.clone())),
                MountFlags::default(),
                MountRestrictions::default(),
            )
            .unwrap();
            let mut nodes = [Some(mount_index::prepare(&view).unwrap())];
            mount_publication::Publication::prepare(
                core::slice::from_ref(&view),
                &mut nodes,
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap()
            .publish();
            let jail = OpenedFile::at(view, source.dentry.clone()).unwrap();
            assert_eq!(
                VFS.open_file_at(
                    LookupOrigin::new(jail.clone(), jail.clone()),
                    b"../..",
                    &AccessIdentity
                )
                .unwrap()
                .inode_identity()
                .unwrap(),
                f.jail.inode_identity().unwrap()
            );
        }
        #[test]
        fn old_cwd_dirfd_and_magic_links_remain_valid_outside_chroot() {
            let _case = CASE.lock();
            let f = Fixture::new();
            let origin = LookupOrigin::new(f.jail.clone(), f.global.clone());
            assert_eq!(
                VFS.current_directory_path(origin.clone()).unwrap(),
                b"(unreachable)/"
            );
            assert!(
                VFS.open_file_at(origin.clone(), b"outside", &AccessIdentity)
                    .is_ok()
            );
            assert!(matches!(
                VFS.open_file_at(origin, b"/outside", &AccessIdentity),
                Err(FileSystemError::NotFound)
            ));
            let magic = Node::new(InodeType::SymLink, b"");
            *magic.magic.lock() = Some(f.global.clone());
            f.jail_inode.add(b"magic", magic);
            assert!(
                VFS.open_file_at(f.origin(), b"/magic/outside", &AccessIdentity)
                    .is_ok()
            );
            // Remaining absolute ordinary link still restarts at caller root after magic escape.
            f.jail_inode.children.lock().remove(b"magic".as_slice());
            let absolute = Node::new(InodeType::SymLink, b"/file");
            // A target directory outside root can be the relative start; root is unchanged.
            let outside_dir = Node::new(InodeType::Directory, b"");
            outside_dir.add(b"absolute", absolute);
            let opened = OpenedFile::child(outside_dir, f.global.clone(), b"other").unwrap();
            assert_eq!(
                VFS.open_file_at(f.origin().with_start(opened), b"absolute", &AccessIdentity)
                    .unwrap()
                    .inode_identity()
                    .unwrap()
                    .1,
                f.file.id
            );
        }
        #[test]
        fn deep_outside_cwd_has_one_prefix_and_magic_suffix_keeps_trailing_slash() {
            let _case = CASE.lock();
            let f = Fixture::new();
            let a = Node::new(InodeType::Directory, b"");
            let b = Node::new(InodeType::Directory, b"");
            let a = OpenedFile::child(a, f.global.clone(), b"a").unwrap();
            let b = OpenedFile::child(b, a, b"b").unwrap();
            let origin = LookupOrigin::new(f.jail.clone(), b);
            assert_eq!(
                VFS.current_directory_path(origin).unwrap(),
                b"(unreachable)/a/b"
            );
            let magic = Node::new(InodeType::SymLink, b"");
            *magic.magic.lock() = Some(f.global.clone());
            f.jail_inode.add(b"magic", magic);
            assert!(matches!(
                VFS.open_file_at(f.origin(), b"/magic/outside/", &AccessIdentity),
                Err(FileSystemError::NotDirectory)
            ));
            assert!(
                VFS.open_file_at(f.origin(), b"/magic/jail/", &AccessIdentity)
                    .unwrap()
                    .inode()
                    .inode_type()
                    == InodeType::Directory
            );
            f.jail_inode.children.lock().remove(b"magic".as_slice());
        }
        #[test]
        fn root_relative_projection_tracks_real_rename_unlink_and_outside_proc_path() {
            let _case = CASE.lock();
            let f = Fixture::new();
            VFS.dentries.move_entry(
                f.jail.inode_identity().unwrap(),
                b"inner",
                f.inner.inode_identity().unwrap(),
                f.jail.dentry.clone(),
                f.jail.inode_identity().unwrap(),
                b"renamed",
            );
            assert_eq!(VFS.current_directory_path(f.origin()).unwrap(), b"/renamed");
            assert_eq!(f.inner.path(&f.jail, true).unwrap(), b"/renamed");
            assert_eq!(f.global.path(&f.jail, true).unwrap(), b"/");
            VFS.dentries.mark_unlinked(
                f.jail.inode_identity().unwrap(),
                b"renamed",
                f.inner.inode_identity().unwrap(),
            );
            assert!(matches!(
                VFS.current_directory_path(f.origin()),
                Err(FileSystemError::NotFound)
            ));
            assert_eq!(f.inner.path(&f.jail, true).unwrap(), b"/renamed (deleted)");
        }
        #[test]
        fn metadata_failures_are_not_equal_root_identities_and_final_symlink_trailing_slash_is_checked()
         {
            let _case = CASE.lock();
            let f = Fixture::new();
            for path in [b"/file/.".as_slice(), b"/file/..", b"/file/"] {
                assert!(matches!(
                    VFS.open_file_at(f.origin(), path, &AccessIdentity),
                    Err(FileSystemError::NotDirectory)
                ));
            }
            let link = VFS
                .open_file_at_no_follow(f.origin(), b"/absolute", &AccessIdentity)
                .unwrap();
            assert!(link.inode().inode_type() == InodeType::SymLink);
            assert!(matches!(
                VFS.open_file_at_no_follow(f.origin(), b"/absolute/", &AccessIdentity),
                Err(FileSystemError::NotDirectory)
            ));
            f.jail_inode.fail_metadata.store(true, Ordering::Relaxed);
            // Canonical path 身份不需要重新读取 root metadata；从 inner 回到 root 保持正确。
            assert!(
                VFS.open_file_at(f.origin(), b"..", &AccessIdentity)
                    .unwrap()
                    .same_path(&f.jail)
            );
            // 当前 traversal directory 的 metadata/search 失败仍必须返回原始 I/O 错误。
            assert!(matches!(
                VFS.open_file_at(f.origin(), b"/..", &AccessIdentity),
                Err(FileSystemError::IoError)
            ));
        }
        #[test]
        fn component_and_projected_path_limits_report_name_too_long() {
            let _case = CASE.lock();
            let f = Fixture::new();
            let name = [b'x'; 256];
            assert!(matches!(
                VFS.open_file_at(f.origin(), &name, &AccessIdentity),
                Err(FileSystemError::NameTooLong)
            ));
            assert!(matches!(
                VFS.namespace()
                    .unwrap()
                    .parent_from(f.origin(), &name, &AccessIdentity),
                Err(FileSystemError::NameTooLong)
            ));
            let long = [b'x'; 254];
            let mut current = f.jail.clone();
            for _ in 0..16 {
                current = OpenedFile::child(Node::new(InodeType::Directory, b""), current, &long)
                    .unwrap();
            }
            let maximum = LookupOrigin::new(f.jail.clone(), current.clone());
            assert_eq!(VFS.current_directory_path(maximum).unwrap().len(), 4080);
            let over =
                OpenedFile::child(Node::new(InodeType::Directory, b""), current, &[b'y'; 15])
                    .unwrap();
            assert!(matches!(
                VFS.current_directory_path(LookupOrigin::new(f.jail.clone(), over)),
                Err(FileSystemError::NameTooLong)
            ));
        }
        #[test]
        fn magic_target_failure_is_not_retried_as_text() {
            let _case = CASE.lock();
            let f = Fixture::new();
            let link = Node::new(InodeType::SymLink, b"/file");
            *link.magic.lock() = Some(f.inner.clone());
            link.fail_target.store(true, Ordering::Relaxed);
            f.jail_inode.add(b"fault", link);
            assert!(matches!(
                VFS.open_file_at(f.origin(), b"/fault", &AccessIdentity),
                Err(FileSystemError::OutOfMemory)
            ));
            let opened = VFS
                .open_file_at(f.origin(), b"/fault", &AccessIdentity)
                .unwrap();
            assert!(opened.same_path(&f.inner));
        }
        #[test]
        fn lookup_admission_and_rename_publish_one_namespace_transaction() {
            use std::{
                sync::{Barrier, mpsc},
                thread,
            };
            let _case = CASE.lock();
            let f = Fixture::new();
            let pause = Arc::new((Barrier::new(2), Barrier::new(2)));
            *f.jail_inode.find_pause.lock() = Some(pause.clone());
            let origin = f.origin();
            let reader =
                thread::spawn(move || VFS.open_file_at(origin, b"/file", &AccessIdentity).unwrap());
            pause.0.wait(); // Reader holds namespace while storage returns its child.
            let (blocked_tx, blocked_rx) = mpsc::channel();
            let (done_tx, done_rx) = mpsc::channel();
            let jail = f.jail.clone();
            let file = f.file.id;
            let writer = thread::spawn(move || {
                crate::sync::observe_blocking(blocked_tx);
                let _namespace = VFS.namespace().unwrap();
                VFS.dentries.move_entry(
                    jail.inode_identity().unwrap(),
                    b"file",
                    (jail.inode().filesystem_id(), file),
                    jail.dentry.clone(),
                    jail.inode_identity().unwrap(),
                    b"renamed",
                );
                done_tx.send(()).unwrap();
            });
            let blocked = blocked_rx.recv_timeout(std::time::Duration::from_secs(2));
            pause.1.wait();
            let opened = reader.join().unwrap();
            writer.join().unwrap();
            done_rx.recv().unwrap();
            *f.jail_inode.find_pause.lock() = None;
            assert!(
                blocked.is_ok(),
                "writer must encounter the reader namespace guard"
            );
            assert_eq!(
                VFS.namespace()
                    .unwrap()
                    .path(&opened, &f.jail, false)
                    .unwrap(),
                b"/renamed"
            );
        }
        #[test]
        fn whole_path_projection_waits_for_committed_ancestor_move() {
            use std::{sync::mpsc, thread};
            let _case = CASE.lock();
            let f = Fixture::new();
            let transaction = VFS.namespace().unwrap();
            let origin = f.origin();
            let (tx, rx) = mpsc::channel();
            let (blocked_tx, blocked_rx) = mpsc::channel();
            let reader = thread::spawn(move || {
                crate::sync::observe_blocking(blocked_tx);
                tx.send(VFS.current_directory_path(origin).unwrap())
                    .unwrap();
            });
            let blocked = blocked_rx.recv_timeout(std::time::Duration::from_secs(2));
            VFS.dentries.move_entry(
                f.jail.inode_identity().unwrap(),
                b"inner",
                f.inner.inode_identity().unwrap(),
                f.jail.dentry.clone(),
                f.jail.inode_identity().unwrap(),
                b"after",
            );
            drop(transaction);
            let path = rx.recv().unwrap();
            reader.join().unwrap();
            assert!(
                blocked.is_ok(),
                "projection must encounter the mutation namespace guard"
            );
            assert_eq!(path, b"/after");
        }
        #[path = "../../../../tools/kernel-unit/tests/fs_root_lookup/mounts.rs"]
        mod mount_tests;
        #[path = "../../../../tools/kernel-unit/tests/fs_root_lookup/propagation.rs"]
        mod propagation_tests;
        #[test]
        #[ignore = "diagnostic host benchmark; run alone in release"]
        fn actual_root_lookup_and_projection_cost() {
            use std::{hint::black_box, time::Instant};
            let _case = CASE.lock();
            let f = Fixture::new();
            for (name, path) in [
                ("rooted absolute lookup", b"/inner".as_slice()),
                ("rooted absolute symlink", b"/absolute".as_slice()),
            ] {
                for _ in 0..1000 {
                    black_box(VFS.open_file_at(f.origin(), path, &AccessIdentity).unwrap());
                }
                let mut samples = Vec::new();
                for _ in 0..5 {
                    let now = Instant::now();
                    for _ in 0..10000 {
                        black_box(
                            VFS.open_file_at(f.origin(), black_box(path), &AccessIdentity)
                                .unwrap(),
                        );
                    }
                    samples.push(now.elapsed().as_nanos() as f64 / 10000.0);
                }
                samples.sort_by(f64::total_cmp);
                println!("{name}: median {:.2} ns", samples[2]);
            }
            let mut samples = Vec::new();
            for _ in 0..1000 {
                black_box(VFS.current_directory_path(f.origin()).unwrap());
            }
            for _ in 0..5 {
                let now = Instant::now();
                for _ in 0..10000 {
                    black_box(VFS.current_directory_path(f.origin()).unwrap());
                }
                samples.push(now.elapsed().as_nanos() as f64 / 10000.0);
            }
            samples.sort_by(f64::total_cmp);
            println!("rooted cwd projection: median {:.2} ns", samples[2]);
        }
    }
    pub(crate) use vfs::OpenedFile;
}
