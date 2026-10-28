#![feature(allocator_ext)]
#![allow(dead_code)]
//! Actual nsfs/proc per-open operations and namespace owners; dentry, task lookup and user-copy are seams.
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/shm_backing_fixture.rs"]
mod memory;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../src/socket_core_fixture.rs"]
mod socket;
mod timer {
    pub(crate) fn get_time_ns() -> u64 {
        2_000_000_000
    }
    pub(crate) fn get_realtime_ns() -> u64 {
        11_000_000_000
    }
}
// 本fixture不执行mount poll；调度/namespace拓扑在实际VFS专用fixture覆盖。
#[path = "../src/sync.rs"]
mod sync;
mod ipc {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/ipc_namespace_fixture.rs"
    ));
    pub(crate) struct Pipe;
}

#[path = "../../../kernel/src/fs"]
mod fs {
    #[path = "inotify/limits.rs"]
    mod inotify_limits;
    use alloc::{sync::Arc, vec::Vec};
    pub(crate) use inotify_limits::InotifyLimit;
    pub(crate) const O_RDONLY: u32 = 0;
    pub(crate) const O_ACCMODE: u32 = 3;
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum FileSystemError {
        InvalidOperation,
        NoSpace,
        OutOfMemory,
        NotFound,
        NoDevice,
        AccessDenied,
        PermissionDenied,
        NotDirectory,
        IsDirectory,
    }
    mod permission;
    use permission::{AccessIdentity, CreateMetadata, OwnerModeChange};
    #[path = "vfs/mount_limit.rs"]
    mod mount_limit;
    use mount_limit::{mount_limit, set_mount_limit};
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum MountTableKind {
        Mounts,
        Info,
        Stats,
    }
    pub(crate) struct MountNamespace;
    impl MountNamespace {
        pub(crate) fn event(&self) -> u64 {
            unreachable!("mount event seam belongs to fs_root_lookup fixture")
        }
        pub(crate) fn notification_pipe(&self) -> Arc<crate::ipc::Pipe> {
            unreachable!("mount poll seam")
        }
        pub(crate) fn drain_notification(&self) {
            unreachable!("mount poll seam")
        }
    }
    struct MountTableSource;
    fn vfs() -> MountTableSource {
        MountTableSource
    }
    impl MountTableSource {
        fn mount_table(
            &self,
            _: &Arc<MountNamespace>,
            _: &Arc<OpenedFile>,
            _: MountTableKind,
        ) -> Result<Vec<u8>, FileSystemError> {
            unreachable!("mount table seam belongs to fs_root_lookup fixture")
        }
    }
    mod inode_identity;
    mod inode_times;
    pub(crate) enum DataBacking {
        Snapshot,
    }
    pub(crate) struct DirectoryRead;
    pub(crate) trait DirectoryVisitor {}
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub(crate) struct FineTimes {
        pub(crate) atime_nsec: u32,
        pub(crate) mtime_nsec: u32,
        pub(crate) ctime_nsec: u32,
        pub(crate) birth: Option<(u64, u32)>,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum InodeType {
        File,
        Directory,
        SymLink,
    }
    pub(crate) struct InodeMetadata {
        pub(crate) filesystem: u64,
        pub(crate) inode: u64,
        pub(crate) kind: InodeType,
        pub(crate) mode: u32,
        pub(crate) links: u32,
        pub(crate) uid: u32,
        pub(crate) gid: u32,
        pub(crate) size: u64,
        pub(crate) blocks: u64,
        pub(crate) block_size: u32,
        pub(crate) atime: u64,
        pub(crate) mtime: u64,
        pub(crate) ctime: u64,
        pub(crate) fine_times: FineTimes,
        pub(crate) device: Option<()>,
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct SuperblockFacts {
        pub(crate) type_name: &'static str,
        pub(crate) read_only: bool,
    }
    pub(crate) struct FileSystemStatistics {
        pub(crate) magic: u64,
        pub(crate) block_size: u64,
        pub(crate) blocks: u64,
        pub(crate) blocks_free: u64,
        pub(crate) blocks_available: u64,
        pub(crate) files: u64,
        pub(crate) files_free: u64,
        pub(crate) fsid: [u32; 2],
        pub(crate) name_length: u64,
        pub(crate) fragment_size: u64,
        pub(crate) flags: u64,
    }
    pub(crate) enum InodeOpen {
        Device(Arc<dyn chardev::DeviceFile>),
    }
    pub(crate) struct InodeOpenError;
    impl From<FileSystemError> for InodeOpenError {
        fn from(_: FileSystemError) -> Self {
            Self
        }
    }
    pub(crate) trait Inode: Send + Sync {
        fn require_access(
            &self,
            identity: &AccessIdentity,
            requested: u8,
        ) -> Result<(), FileSystemError> {
            identity.require(self.metadata()?, requested)
        }
        fn touch_access(&self, _: u64, _: bool) -> Result<(), FileSystemError> {
            Ok(())
        }
        fn filesystem_id(&self) -> usize;
        fn metadata(&self) -> Result<InodeMetadata, FileSystemError>;
        fn inode_type(&self) -> InodeType;
        fn open_backend(
            &self,
            _flags: u32,
            _identity: &AccessIdentity,
            _authority: &crate::namespace::NamespaceAuthority,
        ) -> Result<InodeOpen, InodeOpenError> {
            Err(FileSystemError::InvalidOperation.into())
        }
        fn namespace_reference(&self) -> Option<crate::namespace::NamespaceReference> {
            None
        }
        fn size(&self) -> u64;
        fn is_executable(&self) -> bool;
        fn data_backing(&self) -> DataBacking;
        fn pseudo_statistics(&self) -> Option<FileSystemStatistics> {
            None
        }
        fn ioctl(&self, _: &chardev::IoctlCall<'_>) -> Result<isize, chardev::DeviceError> {
            Err(chardev::DeviceError::Errno(syscall_abi::errno::ENOTTY))
        }
        fn read_storage(&self, offset: u64, bytes: &mut [u8]) -> Result<usize, FileSystemError>;
        fn write_storage(
            &self,
            offset: u64,
            bytes: &[u8],
            identity: Option<&AccessIdentity>,
        ) -> Result<usize, FileSystemError>;
        fn append_storage(
            &self,
            bytes: &[u8],
            identity: Option<&AccessIdentity>,
        ) -> Result<(u64, usize), FileSystemError>;
        fn truncate_storage(
            &self,
            length: u64,
            identity: Option<&AccessIdentity>,
        ) -> Result<(), FileSystemError>;
        fn set_times(&self, _: Option<u64>, _: Option<u64>) -> Result<(), FileSystemError> {
            Err(FileSystemError::PermissionDenied)
        }
        fn change_owner_mode(&self, _: OwnerModeChange) -> Result<(), FileSystemError> {
            Err(FileSystemError::PermissionDenied)
        }
        fn sync_storage(&self) -> Result<(), FileSystemError>;
        fn read_directory(
            &self,
            offset: u64,
            visitor: &mut dyn DirectoryVisitor,
        ) -> Result<DirectoryRead, FileSystemError>;
        fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError>;
        fn create(
            &self,
            name: &[u8],
            kind: InodeType,
            metadata: CreateMetadata,
        ) -> Result<Arc<dyn Inode>, FileSystemError>;
        fn unlink(&self, name: &[u8], directory: bool) -> Result<(), FileSystemError>;
        fn rename(
            &self,
            name: &[u8],
            directory: u64,
            new_name: &[u8],
            replace: bool,
        ) -> Result<(), FileSystemError>;
    }
    // Path shell是显式VFS边界adapter；实际Weak dentry admission属于生产NamespaceInode。
    pub(crate) struct OpenedFile {
        entry: Arc<vfs::Dentry>,
        mount: Arc<vfs::MountedFilesystem>,
    }
    impl OpenedFile {
        pub(in crate::fs) fn with_dentry(
            &self,
            entry: Arc<vfs::Dentry>,
        ) -> Result<Arc<Self>, FileSystemError> {
            Arc::try_new(Self {
                entry,
                mount: self.mount.clone(),
            })
            .map_err(|_| FileSystemError::OutOfMemory)
        }
        fn inode(&self) -> Arc<dyn Inode> {
            self.entry.inode.clone()
        }
        fn label(&self) -> &[u8] {
            &self.entry.label
        }
    }
    pub(crate) trait FileSystem: Send + Sync {
        fn superblock(&self) -> SuperblockFacts;
        fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError>;
        fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError>;
    }
    mod vfs {
        use super::*;
        pub(in crate::fs) struct Dentry {
            pub(super) inode: Arc<dyn Inode>,
            pub(super) label: Vec<u8>,
        }
        impl Dentry {
            pub(in crate::fs) fn inode(&self) -> Arc<dyn Inode> {
                self.inode.clone()
            }
            pub(in crate::fs) fn anonymous(
                inode: Arc<dyn Inode>,
                label: Vec<u8>,
            ) -> Result<Arc<Self>, FileSystemError> {
                Arc::try_new(Self { inode, label }).map_err(|_| FileSystemError::OutOfMemory)
            }
        }
        // 实际adapter与空root原样加载；superblock registration/gate在fs_root_lookup覆盖。
        pub(in crate::fs) struct MountedFilesystem {
            pub(super) filesystem: Arc<dyn FileSystem>,
        }
        impl MountedFilesystem {
            pub(in crate::fs) fn new(
                filesystem: Arc<dyn FileSystem>,
                _: Option<()>,
                _: bool,
                _: Arc<crate::namespace::UserNamespace>,
            ) -> Result<(Arc<Self>, Arc<dyn Inode>), FileSystemError> {
                let root = filesystem.root_inode()?;
                let owner =
                    Arc::try_new(Self { filesystem }).map_err(|_| FileSystemError::OutOfMemory)?;
                Ok((owner, root))
            }
        }
        pub(super) fn internal_mount(
            mount: Arc<MountedFilesystem>,
            root: Arc<dyn Inode>,
            _: Arc<Vec<u8>>,
        ) -> Result<Arc<OpenedFile>, FileSystemError> {
            let entry = Dentry::anonymous(root, Vec::new())?;
            Arc::try_new(OpenedFile { entry, mount }).map_err(|_| FileSystemError::OutOfMemory)
        }
    }
    pub(crate) fn try_format_bytes(
        args: core::fmt::Arguments<'_>,
    ) -> Result<Vec<u8>, FileSystemError> {
        procfs::format_bytes(args)
    }
    pub(crate) mod chardev {
        use crate::namespace::{NamespaceAuthority, NamespaceReference};
        use alloc::sync::Arc;
        #[derive(Debug, PartialEq, Eq)]
        pub(crate) enum DeviceError {
            Errno(isize),
        }
        pub(crate) struct UserFault;
        pub(crate) struct DeviceWaitSources;
        impl DeviceWaitSources {
            pub(crate) fn new() -> Self {
                Self
            }
            pub(crate) fn pipe(_: Arc<crate::ipc::Pipe>, _: i16) -> Self {
                unreachable!("mount poll seam")
            }
        }
        pub(crate) trait UserMemory {
            fn pid_namespace(&self) -> Arc<crate::namespace::PidNamespace>;
            fn install_namespace(
                &self,
                namespace: NamespaceReference,
            ) -> Result<isize, DeviceError>;
            fn write(&self, address: usize, bytes: &[u8]) -> Result<(), UserFault>;
        }
        pub(crate) struct IoctlCall<'a> {
            pub(crate) request: usize,
            pub(crate) argument: usize,
            pub(crate) user: &'a dyn UserMemory,
            pub(crate) fd_readable: bool,
            pub(crate) authority: NamespaceAuthority,
        }
        pub(crate) trait UserOutput {
            fn remaining(&self) -> usize;
            fn write(&mut self, bytes: &[u8]) -> Result<(), UserFault>;
        }
        pub(crate) trait UserInput {
            fn remaining(&self) -> usize;
            fn copy(&self, bytes: &mut [u8]) -> Result<(), UserFault>;
            fn consume(&mut self, length: usize);
        }
        pub(crate) trait PositionedFile {
            fn read_at(&self, output: &mut dyn UserOutput, offset: u64) -> Result<(), DeviceError>;
            fn write_at(
                &self,
                input: &mut dyn UserInput,
                offset: u64,
                writer: &NamespaceAuthority,
            ) -> Result<(), DeviceError>;
            fn seek(&self, current: u64, offset: i64, whence: u32) -> Result<u64, DeviceError>;
        }
        pub(crate) trait DeviceFile: Send + Sync {
            fn supports_read(&self) -> bool {
                true
            }
            fn supports_write(&self) -> bool {
                true
            }
            fn poll(&self, events: i16) -> i16;
            fn wait_sources(&self, _: i16) -> DeviceWaitSources {
                DeviceWaitSources::new()
            }
            fn readiness_generation(&self) -> u64 {
                0
            }
            fn prepare_wait(&self, _: i16) -> Option<Arc<crate::ipc::Pipe>> {
                None
            }
            fn positioned(&self) -> Option<&dyn PositionedFile> {
                None
            }
            fn ioctl(&self, _: &IoctlCall<'_>) -> Result<isize, DeviceError> {
                Err(DeviceError::Errno(syscall_abi::errno::ENOTTY))
            }
        }
    }
    // 加载实际 pseudo 类型与 canonical filesystem/mount owner；VFS 壳为上面的显式 seam。
    mod anonymous {
        #[path = "filesystem_type.rs"]
        mod filesystem_type;
        pub(super) use filesystem_type::PseudoFileSystem;
        mod filesystem;
    }
    mod nsfs;
    use nsfs::namespace_opened;
    mod procfs {
        use super::*;
        use crate::fs::chardev::DeviceError;
        use crate::namespace::{NamespaceAuthority, UserNamespace, UserNamespaceMap};
        pub(super) mod text;
        use text::ProcText;
        pub(super) fn format_bytes(
            args: core::fmt::Arguments<'_>,
        ) -> Result<Vec<u8>, FileSystemError> {
            text::proc_text(args)
        }
        mod attributes;
        mod identity;
        mod ipc_description;
        mod opened_operations;
        mod sysctl_limit;
        mod sysctl_permissions;
        use super::{InodeMetadata, InodeType};
        mod networking;
        mod node;
        use node::ProcNode;
        struct Credentials {
            user_namespace: Arc<UserNamespace>,
        }
        trait ProcSource: Send + Sync {
            fn network_snapshot(
                &self,
                target: &crate::socket::NetNamespace,
            ) -> Result<Vec<networking::ProcNetworkSnapshot>, FileSystemError>;
            fn route_snapshot(
                &self,
                target: &crate::socket::NetNamespace,
            ) -> Result<Vec<networking::ProcRouteSnapshot>, FileSystemError>;
            fn thread_time_for_children(
                &self,
                tid: usize,
            ) -> Option<Arc<crate::namespace::TimeNamespace>>;
        }
        mod time_offsets;
        struct Source(
            spin::Mutex<Arc<UserNamespace>>,
            spin::Mutex<Option<Arc<crate::namespace::TimeNamespace>>>,
            spin::Mutex<Arc<crate::ipc::IpcNamespace>>,
            spin::Mutex<Arc<crate::namespace::PidNamespace>>,
        );
        impl ProcSource for Source {
            fn network_snapshot(
                &self,
                _: &crate::socket::NetNamespace,
            ) -> Result<Vec<networking::ProcNetworkSnapshot>, FileSystemError> {
                unreachable!(
                    "protocol snapshot belongs to actual network endpoint fixture; this seam checks open lifetime only"
                )
            }
            fn route_snapshot(
                &self,
                _: &crate::socket::NetNamespace,
            ) -> Result<Vec<networking::ProcRouteSnapshot>, FileSystemError> {
                unreachable!(
                    "actual FIB belongs to the network endpoint fixture; this seam checks captured open lifetime"
                )
            }
            fn thread_time_for_children(
                &self,
                _: usize,
            ) -> Option<Arc<crate::namespace::TimeNamespace>> {
                self.1.lock().clone()
            }
        }
        impl Source {
            fn current_ipc_namespace(
                &self,
            ) -> Result<Arc<crate::ipc::IpcNamespace>, FileSystemError> {
                Ok(self.2.lock().clone())
            }
            fn current_pid_namespace(&self) -> Arc<crate::namespace::PidNamespace> {
                self.3.lock().clone()
            }
            fn thread_mount_snapshot(
                &self,
                _: usize,
            ) -> Option<(Arc<MountNamespace>, Arc<OpenedFile>)> {
                unreachable!("actual mount snapshot uses fs_root_lookup fixture")
            }
            fn thread_credentials(&self, _: usize) -> Result<Option<Credentials>, FileSystemError> {
                Ok(Some(Credentials {
                    user_namespace: self.0.lock().clone(),
                }))
            }
        }
        struct ProcInode {
            filesystem_id: usize,
            source: Arc<Source>,
            node: ProcNode,
            sysctl_pid_namespace: Option<alloc::sync::Weak<crate::namespace::PidNamespace>>,
            sysctl_ipc_namespace: Option<alloc::sync::Weak<crate::ipc::IpcNamespace>>,
            net_namespace: Option<alloc::sync::Weak<crate::socket::NetNamespace>>,
            user_namespace: Option<alloc::sync::Weak<UserNamespace>>,
        }
        impl ProcInode {
            // Proc source/dentry identity are this fixture's explicit seam; operations own real namespaces.
            fn shared(&self) -> Arc<Self> {
                Arc::new(Self {
                    filesystem_id: self.filesystem_id,
                    source: self.source.clone(),
                    node: self.node,
                    sysctl_pid_namespace: self.sysctl_pid_namespace.clone(),
                    sysctl_ipc_namespace: self.sysctl_ipc_namespace.clone(),
                    net_namespace: self.net_namespace.clone(),
                    user_namespace: self.user_namespace.clone(),
                })
            }
            fn file_contents(&self, _: &Arc<UserNamespace>) -> Result<Vec<u8>, FileSystemError> {
                unreachable!("non-map snapshot seam")
            }
        }
        struct Output(Vec<u8>);
        impl chardev::UserOutput for Output {
            fn remaining(&self) -> usize {
                4096 - self.0.len()
            }
            fn write(&mut self, bytes: &[u8]) -> Result<(), chardev::UserFault> {
                self.0.extend_from_slice(bytes);
                Ok(())
            }
        }
        struct Input<'a> {
            bytes: &'a [u8],
            consumed: usize,
            fault: bool,
        }
        impl chardev::UserInput for Input<'_> {
            fn remaining(&self) -> usize {
                self.bytes.len() - self.consumed
            }
            fn copy(&self, bytes: &mut [u8]) -> Result<(), chardev::UserFault> {
                if self.fault {
                    return Err(chardev::UserFault);
                }
                bytes.copy_from_slice(&self.bytes[self.consumed..self.consumed + bytes.len()]);
                Ok(())
            }
            fn consume(&mut self, length: usize) {
                self.consumed += length;
            }
        }
        fn authority() -> NamespaceAuthority {
            NamespaceAuthority {
                namespace: crate::namespace::boot(),
                effective_uid: 0,
                effective_gid: 0,
                effective_capabilities: syscall_abi::capability::VALID_MASK,
            }
        }
        fn inode(target: Arc<UserNamespace>, kind: UserNamespaceMap) -> ProcInode {
            ProcInode {
                filesystem_id: 1,
                source: Arc::new(Source(
                    spin::Mutex::new(target),
                    spin::Mutex::new(Some(crate::namespace::initial_time())),
                    spin::Mutex::new(
                        crate::ipc::IpcNamespace::create(crate::namespace::boot(), 0).unwrap(),
                    ),
                    spin::Mutex::new(crate::namespace::initial_pid()),
                )),
                node: ProcNode::UserMap(41, kind),
                sysctl_pid_namespace: None,
                sysctl_ipc_namespace: None,
                net_namespace: Some(Arc::downgrade(&crate::socket::boot_net())),
                user_namespace: None,
            }
        }
        fn read(file: &dyn chardev::DeviceFile) -> String {
            let mut output = Output(Vec::new());
            file.positioned().unwrap().read_at(&mut output, 0).unwrap();
            String::from_utf8(output.0).unwrap()
        }
        #[test]
        fn sysvipc_partial_fault_and_removed_object_keep_only_the_actual_record_suffix() {
            struct Limited {
                bytes: Vec<u8>,
                capacity: usize,
                fault_after: Option<usize>,
            }
            impl chardev::UserOutput for Limited {
                fn remaining(&self) -> usize {
                    self.capacity - self.bytes.len()
                }
                fn write(&mut self, bytes: &[u8]) -> Result<(), chardev::UserFault> {
                    let count = self.fault_after.unwrap_or(bytes.len()).min(bytes.len());
                    self.bytes.extend_from_slice(&bytes[..count]);
                    if self.fault_after.is_some() {
                        Err(chardev::UserFault)
                    } else {
                        Ok(())
                    }
                }
            }
            let caller = authority();
            let namespace = crate::ipc::IpcNamespace::create(caller.namespace.clone(), 0).unwrap();
            let access = crate::ipc::IpcAccess {
                authority: caller.clone(),
                filesystem_gid: 0,
                groups: None,
            };
            let id = namespace
                .message_get(120019, syscall_abi::ipc::IPC_CREAT | 0o666, &access, 11)
                .unwrap();
            namespace
                .message_get(120020, syscall_abi::ipc::IPC_CREAT | 0o666, &access, 12)
                .unwrap();
            let mut node = inode(caller.namespace.clone(), UserNamespaceMap::Uid);
            node.node = ProcNode::IpcTable(crate::ipc::IpcTable::Message);
            *node.source.2.lock() = namespace.clone();
            let opened = opened_operations::open(&node, 0, &caller).unwrap();
            let expected = read(opened.as_ref()).into_bytes();
            let mut fault = Limited {
                bytes: Vec::new(),
                capacity: 7,
                fault_after: Some(3),
            };
            assert_eq!(
                opened.positioned().unwrap().read_at(&mut fault, 0),
                Err(chardev::DeviceError::Errno(syscall_abi::errno::EFAULT))
            );
            assert_eq!(fault.bytes, expected[..3]);
            let header = expected.iter().position(|byte| *byte == b'\n').unwrap() + 1;
            let mut partial = Limited {
                bytes: Vec::new(),
                capacity: header + 1 - 3,
                fault_after: None,
            };
            opened
                .positioned()
                .unwrap()
                .read_at(&mut partial, 3)
                .unwrap();
            assert_eq!(partial.bytes, expected[3..header + 1]);
            // 第一条 row 已被真实 codec 缓存；删除实际对象不能让半条记录变成别的对象。
            namespace.message_remove(id, &access).unwrap();
            let mut suffix = Output(Vec::new());
            opened
                .positioned()
                .unwrap()
                .read_at(&mut suffix, (header + 1) as u64)
                .unwrap();
            assert_eq!(suffix.0, expected[header + 1..]);
            let reset = read(opened.as_ref());
            assert!(!reset.contains("120019"));
            assert!(reset.contains("120020"));
        }

        #[test]
        fn sysvipc_fragmented_read_uses_one_actual_cursor_and_no_record_allocation() {
            struct Fragment {
                bytes: [u8; 7],
                used: usize,
            }
            impl chardev::UserOutput for Fragment {
                fn remaining(&self) -> usize {
                    self.bytes.len() - self.used
                }
                fn write(&mut self, bytes: &[u8]) -> Result<(), chardev::UserFault> {
                    self.bytes[self.used..self.used + bytes.len()].copy_from_slice(bytes);
                    self.used += bytes.len();
                    Ok(())
                }
            }
            let caller = authority();
            let namespace = crate::ipc::IpcNamespace::create(caller.namespace.clone(), 0).unwrap();
            let access = crate::ipc::IpcAccess {
                authority: caller.clone(),
                filesystem_gid: 0,
                groups: None,
            };
            namespace
                .message_get(120010, syscall_abi::ipc::IPC_CREAT | 0o666, &access, 11)
                .unwrap();
            namespace
                .message_get(120011, syscall_abi::ipc::IPC_CREAT | 0o666, &access, 12)
                .unwrap();
            let mut node = inode(caller.namespace.clone(), UserNamespaceMap::Uid);
            node.node = ProcNode::IpcTable(crate::ipc::IpcTable::Message);
            *node.source.2.lock() = namespace;
            let opened = opened_operations::open(&node, 0, &caller).unwrap();
            let expected = read(opened.as_ref()).into_bytes();
            let mut actual = Vec::with_capacity(expected.len());
            let duplicate = opened.clone();
            let mut offset = 0u64;
            let mut index = 0usize;
            loop {
                let mut output = Fragment {
                    bytes: [0; 7],
                    used: 0,
                };
                crate::allocation_failure::fail_next();
                let file = if index.is_multiple_of(2) {
                    &opened
                } else {
                    &duplicate
                };
                let result = file.positioned().unwrap().read_at(&mut output, offset);
                let consumed = crate::allocation_failure::was_consumed();
                assert!(!consumed, "sysvipc record/continuation must not allocate");
                result.unwrap();
                if output.used == 0 {
                    break;
                }
                actual.extend_from_slice(&output.bytes[..output.used]);
                offset += output.used as u64;
                index += 1;
            }
            assert_eq!(actual, expected);
            let mut middle = Fragment {
                bytes: [0; 7],
                used: 0,
            };
            opened
                .positioned()
                .unwrap()
                .read_at(&mut middle, 5)
                .unwrap();
            assert_eq!(&middle.bytes, &expected[5..12]);
            assert_eq!(read(opened.as_ref()).into_bytes(), expected);
        }

        #[test]
        fn sysvipc_open_pins_actual_namespace_and_lists_objects_without_object_read_permission() {
            let caller = authority();
            let first = crate::ipc::IpcNamespace::create(caller.namespace.clone(), 0).unwrap();
            let next = crate::ipc::IpcNamespace::create(caller.namespace.clone(), 0).unwrap();
            let mut unprivileged = caller.clone();
            unprivileged.effective_capabilities = 0;
            let access = crate::ipc::IpcAccess {
                authority: unprivileged,
                filesystem_gid: 0,
                groups: None,
            };
            let id = first
                .message_get(120001, syscall_abi::ipc::IPC_CREAT, &access, 7)
                .unwrap();
            assert!(matches!(
                first.message_snapshot(id, false, false, &access),
                Err(crate::ipc::IpcError::Access)
            ));
            next.message_get(120002, syscall_abi::ipc::IPC_CREAT, &access, 8)
                .unwrap();
            let mut node = inode(caller.namespace.clone(), UserNamespaceMap::Uid);
            node.node = ProcNode::IpcTable(crate::ipc::IpcTable::Message);
            *node.source.2.lock() = first.clone();
            let opened = opened_operations::open(&node, 0, &caller).unwrap();
            *node.source.2.lock() = next.clone();
            let text = read(opened.as_ref());
            assert!(text.starts_with("       key      msqid perms"));
            assert!(
                text.lines()
                    .skip(1)
                    .any(|line| line.split_whitespace().next() == Some("120001"))
            );
            assert!(
                !text
                    .lines()
                    .skip(1)
                    .any(|line| line.split_whitespace().next() == Some("120002"))
            );
            let replacement = opened_operations::open(&node, 0, &caller).unwrap();
            assert!(
                read(replacement.as_ref())
                    .lines()
                    .skip(1)
                    .any(|line| line.split_whitespace().next() == Some("120002"))
            );
            first.message_remove(id, &access).unwrap();
            assert_eq!(read(opened.as_ref()).lines().count(), 1);
        }

        #[test]
        fn timens_offsets_follow_target_membership_and_preserve_opener_authority() {
            let caller = authority();
            let initial = crate::namespace::initial_time();
            let time = initial.copy_for(caller.namespace.clone(), 0).unwrap();
            let mut inode = inode(caller.namespace.clone(), UserNamespaceMap::Uid);
            inode.node = ProcNode::TimeOffsets(41);
            *inode.source.1.lock() = Some(time.clone());
            let opened = opened_operations::open(&inode, 2, &caller).unwrap();
            let operations = opened.positioned().unwrap();
            let denied = NamespaceAuthority {
                effective_capabilities: 0,
                ..caller.clone()
            };
            let mut input = Input {
                bytes: b"monotonic -1 250000000\n7 300 0\nignored third line",
                consumed: 0,
                fault: false,
            };
            operations.write_at(&mut input, 0, &denied).unwrap();
            assert_eq!(input.consumed, b"monotonic -1 250000000\n7 300 0\n".len());
            assert_eq!(
                read(opened.as_ref()),
                "monotonic -1 250000000\nboottime 300 0\n"
            );
            let old = time.snapshot();
            for (bytes, offset, fault, error) in [
                (
                    b"1 100 0\n7 -3 0\n".as_slice(),
                    0,
                    false,
                    syscall_abi::errno::ERANGE,
                ),
                (
                    b"1 1 1000000000".as_slice(),
                    0,
                    false,
                    syscall_abi::errno::EINVAL,
                ),
                (b"1 1 0".as_slice(), 1, true, syscall_abi::errno::EINVAL),
                (b"1 1 0".as_slice(), 0, true, syscall_abi::errno::EFAULT),
            ] {
                let mut input = Input {
                    bytes,
                    consumed: 0,
                    fault,
                };
                assert_eq!(
                    operations.write_at(&mut input, offset, &caller),
                    Err(chardev::DeviceError::Errno(error))
                );
                assert_eq!(input.consumed, 0);
                assert_eq!(time.snapshot(), old);
            }
            time.enter();
            let mut input = Input {
                bytes: b"1 100 0",
                consumed: 0,
                fault: false,
            };
            assert_eq!(
                operations.write_at(&mut input, 0, &caller),
                Err(chardev::DeviceError::Errno(syscall_abi::errno::EACCES))
            );
            let replacement = initial.copy_for(caller.namespace.clone(), 0).unwrap();
            *inode.source.1.lock() = Some(replacement.clone());
            operations.write_at(&mut input, 0, &caller).unwrap();
            assert_eq!(replacement.snapshot().monotonic.seconds, 100);
            assert_eq!(time.snapshot(), old);
            let no_cap_open = opened_operations::open(&inode, 2, &denied).unwrap();
            let mut input = Input {
                bytes: b"1 200 0",
                consumed: 0,
                fault: false,
            };
            assert_eq!(
                no_cap_open
                    .positioned()
                    .unwrap()
                    .write_at(&mut input, 0, &caller),
                Err(chardev::DeviceError::Errno(syscall_abi::errno::EPERM))
            );
            *inode.source.1.lock() = None;
            assert_eq!(
                operations.write_at(&mut input, 0, &caller),
                Err(chardev::DeviceError::Errno(syscall_abi::errno::ESRCH))
            );
        }
        #[test]
        fn core_buffers_read_actual_globals_and_recheck_writer_before_user_copy() {
            let root = authority();
            let denied = NamespaceAuthority {
                effective_uid: 1000,
                effective_capabilities: 0,
                ..root.clone()
            };
            for resource in crate::socket::CoreBufferLimit::ALL {
                let mut node = inode(root.namespace.clone(), UserNamespaceMap::Uid);
                node.node = ProcNode::CoreBufferLimit(resource);
                let file = opened_operations::open(&node, 0, &denied).unwrap();
                assert_eq!(read(&*file), format!("{}\n", resource.value()));
                assert!(matches!(
                    opened_operations::open(&node, 1, &denied),
                    Err(FileSystemError::AccessDenied)
                ));
                let writable = opened_operations::open(&node, 2, &root).unwrap();
                let mut fault = Input {
                    bytes: b"1",
                    consumed: 0,
                    fault: true,
                };
                assert_eq!(
                    writable
                        .positioned()
                        .unwrap()
                        .write_at(&mut fault, 0, &denied),
                    Err(chardev::DeviceError::Errno(syscall_abi::errno::EPERM))
                );
                assert_eq!(fault.consumed, 0);
                let mut output = Output(Vec::new());
                file.positioned().unwrap().read_at(&mut output, 1).unwrap();
                assert!(output.0.is_empty());
                let capable = NamespaceAuthority {
                    effective_uid: 1000,
                    effective_capabilities: syscall_abi::capability::Capability::NET_ADMIN.mask(),
                    ..root.clone()
                };
                assert!(opened_operations::open(&node, 2, &capable).is_ok());
            }
        }
        #[test]
        fn optmem_sysctl_uses_actual_captured_policy_and_current_writer() {
            let root = authority();
            let target = crate::socket::boot_net();
            struct Restore(Arc<crate::socket::NetNamespace>, NamespaceAuthority, u32);
            impl Drop for Restore {
                fn drop(&mut self) {
                    self.0.set_optmem_max(self.2, &self.1).unwrap();
                }
            }
            let _restore = Restore(target.clone(), root.clone(), target.optmem_max() as u32);
            let mut node = inode(root.namespace.clone(), UserNamespaceMap::Uid);
            node.node = ProcNode::OptmemLimit;
            let file = opened_operations::open(&node, 2, &root).unwrap();
            let mut value = Input {
                bytes: b"0x40\n",
                consumed: 0,
                fault: false,
            };
            file.positioned()
                .unwrap()
                .write_at(&mut value, 0, &root)
                .unwrap();
            assert_eq!(target.optmem_max(), 64);
            assert_eq!(read(&*file), "64\n");
            let mut ignored = Input {
                bytes: b"garbage",
                consumed: 0,
                fault: false,
            };
            file.positioned()
                .unwrap()
                .write_at(&mut ignored, 1, &root)
                .unwrap();
            assert_eq!(ignored.consumed, 7);
            assert_eq!(target.optmem_max(), 64);
            let denied = NamespaceAuthority {
                effective_uid: 1000,
                effective_capabilities: 0,
                ..root.clone()
            };
            assert!(matches!(
                opened_operations::open(&node, 2, &denied),
                Err(FileSystemError::AccessDenied)
            ));
            let mut fault = Input {
                bytes: b"1",
                consumed: 0,
                fault: true,
            };
            assert_eq!(
                file.positioned().unwrap().write_at(&mut fault, 0, &denied),
                Err(chardev::DeviceError::Errno(syscall_abi::errno::EPERM))
            );
            assert_eq!(
                file.positioned().unwrap().write_at(&mut fault, 0, &root),
                Err(chardev::DeviceError::Errno(syscall_abi::errno::EFAULT))
            );
            assert_eq!(target.optmem_max(), 64);
            let mut invalid = Input {
                bytes: b"2147483648\n",
                consumed: 0,
                fault: false,
            };
            assert_eq!(
                file.positioned().unwrap().write_at(&mut invalid, 0, &root),
                Err(chardev::DeviceError::Errno(syscall_abi::errno::EINVAL))
            );
            assert_eq!(target.optmem_max(), 64);
            let mut zero = Input {
                bytes: b"0\n",
                consumed: 0,
                fault: false,
            };
            file.positioned()
                .unwrap()
                .write_at(&mut zero, 0, &root)
                .unwrap();
            assert_eq!(read(&*file), "0\n");
        }
        #[test]
        fn network_table_open_pins_exact_child_domain_without_task_or_caller_fallback() {
            let root = authority();
            crate::socket::boot_net();
            let first = crate::socket::NetNamespace::create(root.namespace.clone(), 88372).unwrap();
            let second =
                crate::socket::NetNamespace::create(root.namespace.clone(), 88372).unwrap();
            let retired = Arc::downgrade(&first);
            let mut node = inode(root.namespace.clone(), UserNamespaceMap::Uid);
            node.node = ProcNode::NetDev;
            node.net_namespace = Some(retired.clone());
            assert!(matches!(
                opened_operations::open(&node, 2, &root),
                Err(FileSystemError::AccessDenied)
            ));
            let file = opened_operations::open(&node, 0, &root).unwrap();
            assert_eq!(Arc::strong_count(&first), 2);
            node.net_namespace = Some(Arc::downgrade(&second));
            drop(first);
            assert!(retired.upgrade().is_some());
            assert_eq!(Arc::strong_count(&second), 1);
            drop(file);
            assert!(retired.upgrade().is_none());
            node.net_namespace = Some(retired);
            assert!(matches!(
                opened_operations::open(&node, 0, &root),
                Err(FileSystemError::NoDevice)
            ));
            node.net_namespace = None;
            assert!(matches!(
                opened_operations::open(&node, 0, &root),
                Err(FileSystemError::NoDevice)
            ));
        }
        #[test]
        fn network_formatter_preserves_all_link_names_and_actual_drop_counts() {
            use networking::{NetworkTable, ProcNetworkSnapshot, contents};
            let row = |name: &[u8]| {
                let mut link = [0; 16];
                link[..name.len()].copy_from_slice(name);
                ProcNetworkSnapshot {
                    name: link,
                    received_bytes: 99,
                    received_packets: 2,
                    received_errors: 5,
                    received_dropped: 6,
                    transmitted_bytes: 73,
                    transmitted_packets: 3,
                    transmitted_errors: 7,
                    transmitted_dropped: 17,
                }
            };
            let rows = [row(b"lo"), row(b"eth0"), row(b"uplink\xff")];
            let bytes = contents(NetworkTable::Devices, &rows, &[]).unwrap();
            assert!(bytes.windows(3).any(|part| part == b"lo:"));
            assert!(bytes.windows(5).any(|part| part == b"eth0:"));
            assert!(bytes.windows(8).any(|part| part == b"uplink\xff:"));
            let line = bytes
                .split(|byte| *byte == b'\n')
                .find(|line| line.starts_with(b"uplink\xff:"))
                .unwrap();
            let values = core::str::from_utf8(&line[8..])
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>();
            assert_eq!(values.len(), 16);
            assert_eq!(values[..4], ["99", "2", "5", "6"]);
            assert_eq!(values[8..12], ["73", "3", "7", "17"]);
            let routes = contents(
                NetworkTable::Routes,
                &[],
                &[
                    networking::ProcRouteSnapshot {
                        mtu: 0,
                        window: 0,
                        initial_rtt: 0,
                        name: rows[2].name,
                        destination: [10, 0, 3, 0],
                        gateway: [0; 4],
                        prefix_length: 24,
                        metric: 77,
                        flags: 1,
                    },
                    networking::ProcRouteSnapshot {
                        mtu: 0,
                        window: 0,
                        initial_rtt: 0,
                        name: rows[2].name,
                        destination: [0; 4],
                        gateway: [10, 0, 3, 1],
                        prefix_length: 0,
                        metric: 99,
                        flags: 3,
                    },
                ],
            )
            .unwrap();
            assert!(!routes.windows(3).any(|part| part == b"lo\t"));
            assert!(!routes.windows(5).any(|part| part == b"eth0\t"));
            assert!(routes.windows(8).any(|part| part == b"uplink\xff\t"));
            assert!(routes.windows(8).any(|part| part == b"0003000A"));
            assert!(routes.windows(8).any(|part| part == b"0103000A"));
            assert!(routes.windows(7).any(|part| part == b"\t0\t0\t77"));
            assert!(
                contents(NetworkTable::Devices, &[], &[])
                    .unwrap()
                    .ends_with(b"compressed\n")
            );
        }
        #[test]
        fn distinct_network_sysctl_opens_capture_policy_and_global_buffers_are_readonly() {
            let root = authority();
            crate::socket::boot_net();
            let first = crate::socket::NetNamespace::create(root.namespace.clone(), 88371).unwrap();
            let second =
                crate::socket::NetNamespace::create(root.namespace.clone(), 88371).unwrap();
            let mut node = inode(root.namespace.clone(), UserNamespaceMap::Uid);
            node.node = ProcNode::OptmemLimit;
            node.net_namespace = Some(Arc::downgrade(&first));
            let file = opened_operations::open(&node, 2, &root).unwrap();
            node.net_namespace = Some(Arc::downgrade(&second));
            let next = opened_operations::open(&node, 2, &root).unwrap();
            let mut value = Input {
                bytes: b"41\n",
                consumed: 0,
                fault: false,
            };
            file.positioned()
                .unwrap()
                .write_at(&mut value, 0, &root)
                .unwrap();
            assert_eq!(first.optmem_max(), 41);
            assert_eq!(second.optmem_max(), 128 * 1024);
            assert_eq!(read(&*file), "41\n");
            assert_eq!(read(&*next), "131072\n");
            for resource in crate::socket::CoreBufferLimit::ALL {
                node.node = ProcNode::CoreBufferLimit(resource);
                assert!(matches!(
                    opened_operations::open(&node, 2, &root),
                    Err(FileSystemError::AccessDenied)
                ));
                let readonly = opened_operations::open(&node, 0, &root).unwrap();
                assert_eq!(read(&*readonly), format!("{}\n", resource.value()));
                assert_eq!(
                    readonly
                        .positioned()
                        .unwrap()
                        .write_at(&mut value, 0, &root),
                    Err(chardev::DeviceError::Errno(syscall_abi::errno::EPERM))
                );
            }
        }
        #[test]
        fn mount_max_uses_actual_policy_current_euid_and_has_no_inode_dac_override() {
            struct Restore(u32);
            impl Drop for Restore {
                fn drop(&mut self) {
                    set_mount_limit(self.0).unwrap();
                }
            }
            let _restore = Restore(mount_limit());
            let caller = authority();
            let mut node = inode(caller.namespace.clone(), UserNamespaceMap::Uid);
            node.node = ProcNode::MountLimit;
            let file = opened_operations::open(&node, 2, &caller).unwrap();
            let operations = file.positioned().unwrap();
            let mut input = Input {
                bytes: b"0x20\nignored",
                consumed: 0,
                fault: false,
            };
            operations.write_at(&mut input, 0, &caller).unwrap();
            assert_eq!(input.consumed, 5);
            assert_eq!(mount_limit(), 32);
            assert_eq!(read(&*file), "32\n");
            let mut bad = Input {
                bytes: b"0",
                consumed: 0,
                fault: false,
            };
            assert_eq!(
                operations.write_at(&mut bad, 0, &caller),
                Err(chardev::DeviceError::Errno(syscall_abi::errno::EINVAL))
            );
            assert_eq!(mount_limit(), 32);
            let mut ignored = Input {
                bytes: b"garbage",
                consumed: 0,
                fault: false,
            };
            operations.write_at(&mut ignored, 1, &caller).unwrap();
            assert_eq!(ignored.consumed, 7);
            assert_eq!(mount_limit(), 32);
            let root_euid = AccessIdentity::new(1000, 0, None, caller.namespace.clone(), 0, 0);
            assert_eq!(
                sysctl_permissions::require(root_euid.effective_uid(), 2),
                Ok(())
            );
            let nonroot_euid = AccessIdentity::new(
                0,
                0,
                None,
                caller.namespace.clone(),
                syscall_abi::capability::VALID_MASK,
                1000,
            );
            assert_eq!(
                sysctl_permissions::require(nonroot_euid.effective_uid(), 2),
                Err(FileSystemError::AccessDenied)
            );
            let mut denied = caller.clone();
            denied.effective_uid = 1000;
            assert!(matches!(
                opened_operations::open(&node, 2, &denied),
                Err(FileSystemError::AccessDenied)
            ));
            let mut value = Input {
                bytes: b"7\n",
                consumed: 0,
                fault: false,
            };
            assert_eq!(
                operations.write_at(&mut value, 0, &denied),
                Err(chardev::DeviceError::Errno(syscall_abi::errno::EPERM))
            );
            denied.namespace = UserNamespace::child(
                &caller,
                true,
                crate::namespace::NamespaceRlimits {
                    locked_memory: u64::MAX,
                    pending_signals: u64::MAX,
                },
            )
            .unwrap();
            assert_eq!(
                operations.write_at(&mut value, 0, &denied),
                Err(chardev::DeviceError::Errno(syscall_abi::errno::EPERM))
            );
            assert_eq!(value.consumed, 0);
            assert_eq!(mount_limit(), 32);
            let mut allowed = caller;
            allowed.effective_capabilities = 0;
            operations.write_at(&mut value, 0, &allowed).unwrap();
            assert_eq!(mount_limit(), 7);
            assert_eq!(value.consumed, 2);
        }
        fn attribute_metadata(node: ProcNode) -> InodeMetadata {
            InodeMetadata {
                filesystem: 1,
                inode: 1,
                kind: node.kind(),
                mode: node.metadata_mode(),
                links: 1,
                uid: 0,
                gid: 0,
                size: node.metadata_size(),
                blocks: 0,
                block_size: 4096,
                atime: 0,
                mtime: 0,
                ctime: 0,
                fine_times: Default::default(),
                device: None,
            }
        }
        #[test]
        fn actual_proc_attributes_accept_truncate_without_rewriting_control_size_or_values() {
            let target = crate::namespace::boot();
            let before = target.resource_limit(crate::namespace::UserResource::User);
            let node = ProcNode::UserResourceLimit(crate::namespace::UserResource::User);
            let attributes =
                attributes::ProcAttributes::new(node, attribute_metadata(node)).unwrap();
            attributes.set_times(Some(2), Some(3));
            attributes.truncate();
            let metadata = attributes.snapshot(attribute_metadata(node));
            assert_eq!(
                (
                    metadata.size,
                    metadata.atime,
                    metadata.mtime,
                    metadata.ctime
                ),
                (0, 2, 11, 11)
            );
            assert_eq!(
                target.resource_limit(crate::namespace::UserResource::User),
                before
            );
            assert_eq!(
                attributes.change(
                    node,
                    &super::OwnerModeChange::chown(super::AccessIdentity::root(), Some(42), None)
                ),
                Err(FileSystemError::PermissionDenied)
            );
            assert_eq!(
                attributes.change(
                    node,
                    &super::OwnerModeChange::chmod(super::AccessIdentity::root(), 0o666)
                ),
                Err(FileSystemError::PermissionDenied)
            );
        }
        #[test]
        fn actual_pid_attributes_authorize_chown_and_refresh_live_owner_without_freezing_credentials()
         {
            crate::namespace::boot();
            let node = ProcNode::UserMap(41, UserNamespaceMap::Uid);
            let attributes =
                attributes::ProcAttributes::new(node, attribute_metadata(node)).unwrap();
            attributes
                .change(
                    node,
                    &super::OwnerModeChange::chown(
                        super::AccessIdentity::root(),
                        Some(42),
                        Some(43),
                    ),
                )
                .unwrap();
            let metadata = attributes.snapshot(attribute_metadata(node));
            assert_eq!((metadata.uid, metadata.gid), (42, 43));
            attributes.refresh_owner(101, 102);
            let metadata = attributes.snapshot(attribute_metadata(node));
            assert_eq!((metadata.uid, metadata.gid, metadata.size), (101, 102, 0));
            assert_eq!(
                attributes.change(
                    node,
                    &super::OwnerModeChange::chmod(super::AccessIdentity::root(), 0o666)
                ),
                Err(FileSystemError::PermissionDenied)
            );
        }
        #[test]
        fn actual_generic_proc_registration_preserves_mode_and_owner_after_last_inode_drop() {
            crate::namespace::boot();
            attributes::register_entries().unwrap();
            let node = ProcNode::MemInfo;
            let original = attributes::ProcAttributes::new(node, attribute_metadata(node)).unwrap();
            original
                .change(
                    node,
                    &super::OwnerModeChange::chmod(super::AccessIdentity::root(), 0o640),
                )
                .unwrap();
            original
                .change(
                    node,
                    &super::OwnerModeChange::chown(
                        super::AccessIdentity::root(),
                        Some(42),
                        Some(43),
                    ),
                )
                .unwrap();
            original.set_times(Some(2), Some(3));
            drop(original);
            let reopened = attributes::ProcAttributes::new(node, attribute_metadata(node)).unwrap();
            let metadata = reopened.snapshot(attribute_metadata(node));
            assert_eq!(
                (
                    metadata.mode,
                    metadata.uid,
                    metadata.gid,
                    metadata.atime,
                    metadata.mtime
                ),
                (0o100640, 42, 43, 11, 11)
            );
            reopened
                .change(
                    node,
                    &super::OwnerModeChange::chown(super::AccessIdentity::root(), Some(0), Some(0)),
                )
                .unwrap();
            reopened
                .change(
                    node,
                    &super::OwnerModeChange::chmod(super::AccessIdentity::root(), 0o444),
                )
                .unwrap();
        }
        #[test]
        fn actual_proc_identity_admits_full_pid_fd_and_namespace_keys() {
            use crate::namespace::UserResource;
            let registry = identity::ProcIdentities::<u64>::new();
            let mut pins = Vec::new();
            let mut numbers = std::collections::BTreeSet::new();
            for pid in [1, 2, 0x3fff_ffff, usize::MAX - 1] {
                for fd in [0, 1023, 1024, (1 << 20) - 1, usize::MAX] {
                    let key = (ProcNode::TaskFd(pid, fd), 0);
                    let number = registry.allocate_number().unwrap();
                    assert!(numbers.insert(number));
                    let candidate = Arc::new(number);
                    let selected = registry.live.admit(key, &candidate).unwrap();
                    assert!(Arc::ptr_eq(&selected, &registry.live.get(key).unwrap()));
                    pins.push(selected);
                }
            }
            for key in [
                (ProcNode::UserResourceLimit(UserResource::User), 3),
                (ProcNode::UserResourceLimit(UserResource::User), 9),
                (ProcNode::UserResourceLimit(UserResource::Uts), 9),
                (ProcNode::UserSysctlDir, 9),
                (ProcNode::PidLimit, 3),
                (ProcNode::PidLimit, 9),
            ] {
                let number = registry.allocate_number().unwrap();
                assert!(numbers.insert(number));
                let selected = registry.live.admit(key, &Arc::new(number)).unwrap();
                pins.push(selected);
            }
            let key = (ProcNode::TaskFd(usize::MAX, usize::MAX), 0);
            let first = registry
                .live
                .admit(key, &Arc::new(registry.allocate_number().unwrap()))
                .unwrap();
            let loser = Arc::new(registry.allocate_number().unwrap());
            assert!(Arc::ptr_eq(
                &first,
                &registry.live.admit(key, &loser).unwrap()
            ));
            registry.live.retire(key, loser.as_ref());
            assert!(Arc::ptr_eq(&first, &registry.live.get(key).unwrap()));
            registry.live.retire(key, first.as_ref());
            assert!(registry.live.get(key).is_none());
            let replacement = registry.live.admit(key, &loser).unwrap();
            registry.live.retire(key, first.as_ref());
            assert!(Arc::ptr_eq(&replacement, &registry.live.get(key).unwrap()));
        }
        #[test]
        fn ipc_sem_sysctl_pins_actual_owner_and_commits_standard_signed_prefix() {
            crate::namespace::boot();
            let writer = authority();
            let target = crate::ipc::IpcNamespace::create(writer.namespace.clone(), 0).unwrap();
            let next = crate::ipc::IpcNamespace::create(writer.namespace.clone(), 0).unwrap();
            let mut node = inode(writer.namespace.clone(), UserNamespaceMap::Uid);
            node.node = ProcNode::IpcLimit(crate::ipc::IpcLimit::Semaphores);
            node.sysctl_ipc_namespace = Some(Arc::downgrade(&target));
            let file = opened_operations::open(&node, 2, &writer).unwrap();
            assert_eq!(read(file.as_ref()), "32000\t1024000000\t500\t32000\n");
            let mut value = Input {
                bytes: b"2 3 4 5 extra",
                consumed: 0,
                fault: false,
            };
            file.positioned()
                .unwrap()
                .write_at(&mut value, 0, &writer)
                .unwrap();
            assert_eq!(target.semaphore_limits(), [2, 3, 4, 5]);
            assert_eq!(value.consumed, 8);
            let mut invalid = Input {
                bytes: b"-1 -2 -3 -1",
                consumed: 0,
                fault: false,
            };
            assert!(matches!(
                file.positioned()
                    .unwrap()
                    .write_at(&mut invalid, 0, &writer),
                Err(DeviceError::Errno(syscall_abi::errno::ERANGE))
            ));
            assert_eq!(target.semaphore_limits(), [-1, -2, -3, 5]);
            let mut prefix = Input {
                bytes: b"8 9 garbage",
                consumed: 0,
                fault: false,
            };
            assert!(matches!(
                file.positioned().unwrap().write_at(&mut prefix, 0, &writer),
                Err(DeviceError::Errno(syscall_abi::errno::EINVAL))
            ));
            assert_eq!(target.semaphore_limits(), [8, 9, -3, 5]);
            node.sysctl_ipc_namespace = Some(Arc::downgrade(&next));
            let mut strict = Input {
                bytes: b"10 11 12 13",
                consumed: 0,
                fault: false,
            };
            file.positioned()
                .unwrap()
                .write_at(&mut strict, 1, &writer)
                .unwrap();
            assert_eq!(target.semaphore_limits(), [8, 9, -3, 5]);
            let mut integer = Input {
                bytes: b"-2147483648 0x10 010 0",
                consumed: 0,
                fault: false,
            };
            file.positioned()
                .unwrap()
                .write_at(&mut integer, 0, &writer)
                .unwrap();
            assert_eq!(target.semaphore_limits(), [i32::MIN, 16, 8, 0]);
            assert_eq!(next.semaphore_limits(), [32000, 1024000000, 500, 32000]);
            let mut unprivileged = writer;
            unprivileged.effective_uid = 123;
            let mut value = Input {
                bytes: b"1",
                consumed: 0,
                fault: false,
            };
            assert!(matches!(
                file.positioned()
                    .unwrap()
                    .write_at(&mut value, 0, &unprivileged),
                Err(DeviceError::Errno(syscall_abi::errno::EPERM))
            ));
        }
        #[test]
        fn shm_sysctl_uses_full_native_unsigned_long_and_actual_opened_owner() {
            crate::namespace::boot();
            let writer = authority();
            let target = crate::ipc::IpcNamespace::create(writer.namespace.clone(), 0).unwrap();
            let next = crate::ipc::IpcNamespace::create(writer.namespace.clone(), 0).unwrap();
            let mut node = inode(writer.namespace.clone(), UserNamespaceMap::Uid);
            node.node = ProcNode::IpcLimit(crate::ipc::IpcLimit::SharedMemory(
                crate::ipc::SharedMemoryLimit::MaximumSize,
            ));
            node.sysctl_ipc_namespace = Some(Arc::downgrade(&target));
            let file = opened_operations::open(&node, 2, &writer).unwrap();
            node.sysctl_ipc_namespace = Some(Arc::downgrade(&next));
            let mut maximum = Input {
                bytes: b"18446744073709551615\n",
                consumed: 0,
                fault: false,
            };
            file.positioned()
                .unwrap()
                .write_at(&mut maximum, 0, &writer)
                .unwrap();
            assert_eq!(read(file.as_ref()), "18446744073709551615\n");
            assert_eq!(maximum.consumed, 21);
            assert_eq!(
                target.shared_memory_limit(crate::ipc::SharedMemoryLimit::MaximumSize),
                u64::MAX
            );
            assert_ne!(
                next.shared_memory_limit(crate::ipc::SharedMemoryLimit::MaximumSize),
                u64::MAX
            );
            for bytes in [&b"18446744073709551616"[..], &b"-1"[..]] {
                let mut invalid = Input {
                    bytes,
                    consumed: 0,
                    fault: false,
                };
                assert!(matches!(
                    file.positioned()
                        .unwrap()
                        .write_at(&mut invalid, 0, &writer),
                    Err(DeviceError::Errno(syscall_abi::errno::EINVAL))
                ));
            }
            let mut hex = Input {
                bytes: b"0xffffffff 9",
                consumed: 0,
                fault: false,
            };
            file.positioned()
                .unwrap()
                .write_at(&mut hex, 0, &writer)
                .unwrap();
            assert_eq!(
                target.shared_memory_limit(crate::ipc::SharedMemoryLimit::MaximumSize),
                u32::MAX as u64
            );
            assert_eq!(hex.consumed, 11);
        }
        #[test]
        fn pid_max_fd_pins_actual_namespace_and_rechecks_uid_range_and_position() {
            crate::namespace::boot();
            let writer = authority();
            let target = crate::namespace::initial_pid()
                .child(writer.namespace.clone(), 0)
                .unwrap();
            let mut node = inode(writer.namespace.clone(), UserNamespaceMap::Uid);
            node.node = ProcNode::PidLimit;
            node.sysctl_pid_namespace = Some(Arc::downgrade(&target));
            let file = opened_operations::open(&node, 2, &writer).unwrap();
            let initial = target.limit();
            let mut denied = writer.clone();
            denied.effective_uid = 1000;
            denied.effective_capabilities = 0;
            assert!(matches!(
                opened_operations::open(&node, 2, &denied),
                Err(FileSystemError::AccessDenied)
            ));
            let operations = file.positioned().unwrap();
            for bytes in [
                &b"300\n"[..],
                &b"-1\n"[..],
                &b"4194305\n"[..],
                &b"0x12c\n"[..],
            ] {
                let mut value = Input {
                    bytes,
                    consumed: 0,
                    fault: false,
                };
                assert_eq!(
                    operations.write_at(&mut value, 0, &writer),
                    Err(chardev::DeviceError::Errno(syscall_abi::errno::EINVAL))
                );
                assert_eq!(target.limit(), initial);
                assert_eq!(value.consumed, 0);
            }
            let mut value = Input {
                bytes: b"301\n",
                consumed: 0,
                fault: false,
            };
            assert_eq!(
                operations.write_at(&mut value, 0, &denied),
                Err(chardev::DeviceError::Errno(syscall_abi::errno::EPERM))
            );
            assert_eq!(value.consumed, 0);
            let mut root_without_capability = writer.clone();
            root_without_capability.effective_capabilities = 0;
            operations
                .write_at(&mut value, 0, &root_without_capability)
                .unwrap();
            assert_eq!(target.limit(), 301);
            assert_eq!(read(file.as_ref()), "301\n");
            node.sysctl_pid_namespace = Some(Arc::downgrade(&crate::namespace::initial_pid()));
            assert_eq!(read(file.as_ref()), "301\n");
            let mut value = Input {
                bytes: b"400\n",
                consumed: 0,
                fault: false,
            };
            operations.write_at(&mut value, 1, &writer).unwrap();
            assert_eq!(target.limit(), 301);
            assert_eq!(value.consumed, 4);
            let mut fault = Input {
                bytes: b"400\n",
                consumed: 0,
                fault: true,
            };
            assert_eq!(
                operations.write_at(&mut fault, 0, &writer),
                Err(chardev::DeviceError::Errno(syscall_abi::errno::EFAULT))
            );
            assert_eq!(target.limit(), 301);
        }
        #[test]
        fn inotify_sysctl_uses_one_initial_ledger_and_preserves_scoped_user_limits() {
            use crate::fs::InotifyLimit;
            use crate::namespace::UserResource;
            let root = authority();
            struct Restore(NamespaceAuthority, [u32; 3]);
            impl Drop for Restore {
                fn drop(&mut self) {
                    for (limit, value) in InotifyLimit::ALL.into_iter().zip(self.1) {
                        limit.set(value, &self.0).unwrap();
                    }
                }
            }
            let _restore = Restore(root.clone(), InotifyLimit::ALL.map(InotifyLimit::value));
            let child = UserNamespace::child(
                &root,
                true,
                crate::namespace::NamespaceRlimits {
                    locked_memory: u64::MAX,
                    pending_signals: u64::MAX,
                },
            )
            .unwrap();
            let current = NamespaceAuthority {
                namespace: child.clone(),
                ..root.clone()
            };
            for (global, resource) in [
                (InotifyLimit::Instances, UserResource::InotifyInstances),
                (InotifyLimit::Watches, UserResource::InotifyWatches),
            ] {
                let mut node = inode(root.namespace.clone(), UserNamespaceMap::Uid);
                node.node = ProcNode::InotifyLimit(global);
                assert_eq!(node.node.metadata_mode(), 0o100644);
                let global_file = opened_operations::open(&node, 2, &root).unwrap();
                let mut value = Input {
                    bytes: b"37\n",
                    consumed: 0,
                    fault: false,
                };
                let no_caps = NamespaceAuthority {
                    effective_capabilities: 0,
                    ..root.clone()
                };
                global_file
                    .positioned()
                    .unwrap()
                    .write_at(&mut value, 0, &no_caps)
                    .unwrap();
                assert_eq!(root.namespace.resource_limit(resource), 37);
                assert_eq!(read(global_file.as_ref()), "37\n");
                node.node = ProcNode::UserResourceLimit(resource);
                node.user_namespace = Some(Arc::downgrade(&root.namespace));
                let alias = opened_operations::open(&node, 0, &root).unwrap();
                assert_eq!(
                    read(alias.as_ref()),
                    "37\n",
                    "both global proc paths read the actual same ledger"
                );
                node.user_namespace = Some(Arc::downgrade(&child));
                let child_file = opened_operations::open(&node, 2, &current).unwrap();
                let mut value = Input {
                    bytes: b"0\n",
                    consumed: 0,
                    fault: false,
                };
                child_file
                    .positioned()
                    .unwrap()
                    .write_at(&mut value, 0, &current)
                    .unwrap();
                assert_eq!(child.resource_limit(resource), 0);
                assert_eq!(global.value(), 37);
                *node.source.0.lock() = root.namespace.clone();
                node.user_namespace = Some(Arc::downgrade(&root.namespace));
                let mut value = Input {
                    bytes: b"1\n",
                    consumed: 0,
                    fault: false,
                };
                child_file
                    .positioned()
                    .unwrap()
                    .write_at(&mut value, 0, &current)
                    .unwrap();
                assert_eq!(
                    child.resource_limit(resource),
                    1,
                    "actual open retains its scoped target across membership changes"
                );
                assert_eq!(root.namespace.resource_limit(resource), 37);
                let denied = NamespaceAuthority {
                    effective_uid: 1000,
                    ..root.clone()
                };
                let mut value = Input {
                    bytes: b"12\n",
                    consumed: 0,
                    fault: false,
                };
                assert_eq!(
                    global_file
                        .positioned()
                        .unwrap()
                        .write_at(&mut value, 0, &denied),
                    Err(chardev::DeviceError::Errno(syscall_abi::errno::EPERM))
                );
                assert_eq!(value.consumed, 0);
                node.node = ProcNode::InotifyLimit(global);
                assert!(matches!(
                    opened_operations::open(&node, 2, &denied),
                    Err(FileSystemError::AccessDenied)
                ));
                let dropped = NamespaceAuthority {
                    effective_capabilities: 0,
                    ..current.clone()
                };
                let mut value = Input {
                    bytes: b"12\n",
                    consumed: 0,
                    fault: false,
                };
                assert!(matches!(
                    child_file
                        .positioned()
                        .unwrap()
                        .write_at(&mut value, 0, &dropped),
                    Err(chardev::DeviceError::Errno(syscall_abi::errno::EPERM))
                ));
                assert_eq!(value.consumed, 0);
            }
            let mut node = inode(root.namespace.clone(), UserNamespaceMap::Uid);
            node.node = ProcNode::InotifyLimit(InotifyLimit::QueuedEvents);
            let file = opened_operations::open(&node, 2, &root).unwrap();
            for (bytes, expected) in [
                (b"0\n".as_slice(), 0),
                (b"2147483647\n".as_slice(), i32::MAX as u32),
            ] {
                let mut value = Input {
                    bytes,
                    consumed: 0,
                    fault: false,
                };
                file.positioned()
                    .unwrap()
                    .write_at(&mut value, 0, &root)
                    .unwrap();
                assert_eq!(InotifyLimit::QueuedEvents.value(), expected);
            }
            let mut invalid = Input {
                bytes: b"2147483648\n",
                consumed: 0,
                fault: false,
            };
            assert!(
                file.positioned()
                    .unwrap()
                    .write_at(&mut invalid, 0, &root)
                    .is_err()
            );
            assert_eq!(InotifyLimit::QueuedEvents.value(), i32::MAX as u32);
            let mut ignored = Input {
                bytes: b"garbage",
                consumed: 0,
                fault: false,
            };
            file.positioned()
                .unwrap()
                .write_at(&mut ignored, 1, &root)
                .unwrap();
            assert_eq!(ignored.consumed, 7);
            assert_eq!(InotifyLimit::QueuedEvents.value(), i32::MAX as u32);
        }
        #[test]
        fn sysctl_fd_pins_lookup_namespace_and_rechecks_writer_authority() {
            use crate::namespace::UserResource;
            let parent = authority();
            let child = UserNamespace::child(
                &parent,
                true,
                crate::namespace::NamespaceRlimits {
                    locked_memory: u64::MAX,
                    pending_signals: u64::MAX,
                },
            )
            .unwrap();
            let current = NamespaceAuthority {
                namespace: child.clone(),
                ..parent.clone()
            };
            let mut inode = inode(child.clone(), UserNamespaceMap::Uid);
            inode.node = ProcNode::UserResourceLimit(UserResource::User);
            inode.user_namespace = Some(Arc::downgrade(&child));
            let file = opened_operations::open(&inode, 1, &current).unwrap();
            let mut input = Input {
                bytes: b"0x2\n",
                consumed: 0,
                fault: false,
            };
            file.positioned()
                .unwrap()
                .write_at(&mut input, 0, &current)
                .unwrap();
            assert_eq!(input.consumed, 4);
            assert_eq!(child.resource_limit(UserResource::User), 2);
            assert_eq!(
                parent.namespace.resource_limit(UserResource::User),
                i32::MAX as u32
            );
            // Later source/task membership cannot redirect an already opened control file.
            *inode.source.0.lock() = parent.namespace.clone();
            let mut ignored = Input {
                bytes: b"0\n",
                consumed: 0,
                fault: false,
            };
            file.positioned()
                .unwrap()
                .write_at(&mut ignored, 4, &current)
                .unwrap();
            assert_eq!(ignored.consumed, 2);
            assert_eq!(child.resource_limit(UserResource::User), 2);
            let foreign = NamespaceAuthority {
                effective_capabilities: 0,
                effective_uid: 42,
                ..parent.clone()
            };
            let mut denied = Input {
                bytes: b"1\n",
                consumed: 0,
                fault: false,
            };
            assert!(matches!(
                file.positioned()
                    .unwrap()
                    .write_at(&mut denied, 0, &foreign),
                Err(chardev::DeviceError::Errno(syscall_abi::errno::EPERM))
            ));
            assert_eq!(denied.consumed, 0);
            let mut fault = Input {
                bytes: b"1\n",
                consumed: 0,
                fault: true,
            };
            assert!(matches!(
                file.positioned().unwrap().write_at(&mut fault, 0, &current),
                Err(chardev::DeviceError::Errno(syscall_abi::errno::EFAULT))
            ));
            assert_eq!(child.resource_limit(UserResource::User), 2);
            let mut large = vec![b'x'; 5000];
            large[..2].copy_from_slice(b"3\n");
            let mut input = Input {
                bytes: &large,
                consumed: 0,
                fault: false,
            };
            file.positioned()
                .unwrap()
                .write_at(&mut input, 0, &current)
                .unwrap();
            assert_eq!(input.consumed, 5000 - 4095 + 2);
            assert_eq!(child.resource_limit(UserResource::User), 3);
            let mut ignored = Input {
                bytes: &large,
                consumed: 0,
                fault: false,
            };
            file.positioned()
                .unwrap()
                .write_at(&mut ignored, 9, &current)
                .unwrap();
            assert_eq!(ignored.consumed, 5000);
            let mut restore = Input {
                bytes: b"2\n",
                consumed: 0,
                fault: false,
            };
            file.positioned()
                .unwrap()
                .write_at(&mut restore, 0, &current)
                .unwrap();
            let mut output = Output(Vec::new());
            file.positioned().unwrap().read_at(&mut output, 1).unwrap();
            assert!(
                output.0.is_empty(),
                "numeric sysctl nonzero-position read must be EOF"
            );
            for invalid in [b"-1".as_slice(), b"+1", b"2147483648", b"2x", b"08", b" "] {
                let mut input = Input {
                    bytes: invalid,
                    consumed: 0,
                    fault: false,
                };
                assert!(matches!(
                    file.positioned().unwrap().write_at(&mut input, 0, &current),
                    Err(chardev::DeviceError::Errno(syscall_abi::errno::EINVAL))
                ));
                assert_eq!(input.consumed, 0);
                assert_eq!(child.resource_limit(UserResource::User), 2);
            }
        }
        #[test]
        fn maps_keep_input_order_and_use_opening_namespace_including_initial_unmapped_lower() {
            let parent = authority();
            let child = UserNamespace::child(
                &parent,
                true,
                crate::namespace::NamespaceRlimits {
                    locked_memory: u64::MAX,
                    pending_signals: u64::MAX,
                },
            )
            .unwrap();
            child
                .publish_map(true, b"20 1000 2\n0 2000 2\n", &parent, &parent)
                .unwrap();
            let child_authority = NamespaceAuthority {
                namespace: child.clone(),
                ..parent.clone()
            };
            let map = opened_operations::open(
                &inode(child.clone(), UserNamespaceMap::Uid),
                0,
                &child_authority,
            )
            .unwrap();
            assert_eq!(
                read(map.as_ref()),
                format!(
                    "{:10} {:10} {:10}\n{:10} {:10} {:10}\n",
                    20, 1000, 2, 0, 2000, 2
                )
            );
            let initial = opened_operations::open(
                &inode(parent.namespace.clone(), UserNamespaceMap::Uid),
                0,
                &child_authority,
            )
            .unwrap();
            assert_eq!(
                read(initial.as_ref()),
                format!("{:10} {:10} {:10}\n", 0, u32::MAX, u32::MAX)
            );
            assert_eq!(child.to_kernel(true, 1), Some(2001));
            assert_eq!(child.map_kernel_id(true, 1001), Some(21));
        }
        #[test]
        fn opened_map_pins_target_and_opener_authority_across_target_namespace_change() {
            let parent = authority();
            let child = UserNamespace::child(
                &parent,
                true,
                crate::namespace::NamespaceRlimits {
                    locked_memory: u64::MAX,
                    pending_signals: u64::MAX,
                },
            )
            .unwrap();
            let inode = inode(child.clone(), UserNamespaceMap::Uid);
            let file = opened_operations::open(&inode, 2, &parent).unwrap();
            *inode.source.0.lock() = UserNamespace::child(
                &parent,
                true,
                crate::namespace::NamespaceRlimits {
                    locked_memory: u64::MAX,
                    pending_signals: u64::MAX,
                },
            )
            .unwrap();
            let mut input = Input {
                bytes: b"0 1000 1\n",
                consumed: 0,
                fault: false,
            };
            file.positioned()
                .unwrap()
                .write_at(&mut input, 0, &parent)
                .unwrap();
            assert_eq!(input.consumed, input.bytes.len());
            assert_eq!(child.to_kernel(true, 0), Some(1000));
            assert_eq!(inode.source.0.lock().to_kernel(true, 0), None);
            assert_eq!(file.positioned().unwrap().seek(7, -3, 1), Ok(4));
            assert_eq!(
                file.positioned().unwrap().seek(0, 0, 2),
                Err(chardev::DeviceError::Errno(22))
            );
        }
        #[test]
        fn map_copy_fault_and_nonzero_offset_leave_map_and_input_unpublished() {
            let parent = authority();
            let child = UserNamespace::child(
                &parent,
                true,
                crate::namespace::NamespaceRlimits {
                    locked_memory: u64::MAX,
                    pending_signals: u64::MAX,
                },
            )
            .unwrap();
            let file =
                opened_operations::open(&inode(child.clone(), UserNamespaceMap::Uid), 2, &parent)
                    .unwrap();
            let mut input = Input {
                bytes: b"0 1000 1\n",
                consumed: 0,
                fault: true,
            };
            assert_eq!(
                file.positioned().unwrap().write_at(&mut input, 0, &parent),
                Err(chardev::DeviceError::Errno(14))
            );
            input.fault = false;
            assert_eq!(
                file.positioned().unwrap().write_at(&mut input, 1, &parent),
                Err(chardev::DeviceError::Errno(22))
            );
            assert_eq!(input.consumed, 0);
            assert_eq!(child.to_kernel(true, 0), None);
        }
    }
    #[test]
    fn actual_nsfs_aliases_share_mutable_attributes_and_preserve_namespace_identity() {
        use crate::namespace::{NamespaceAuthority, NamespaceReference, UserNamespace};
        let authority = NamespaceAuthority {
            namespace: crate::namespace::boot(),
            effective_uid: 0,
            effective_gid: 0,
            effective_capabilities: syscall_abi::capability::VALID_MASK,
        };
        let namespace = UserNamespace::child(
            &authority,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX,
            },
        )
        .unwrap();
        let first = namespace_opened(NamespaceReference::new(namespace.clone())).unwrap();
        let second = namespace_opened(NamespaceReference::new(namespace.clone())).unwrap();
        assert!(Arc::ptr_eq(&first.inode(), &second.inode()));
        first
            .inode()
            .change_owner_mode(OwnerModeChange::chmod(AccessIdentity::root(), 0o600))
            .unwrap();
        first
            .inode()
            .change_owner_mode(OwnerModeChange::chown(
                AccessIdentity::root(),
                Some(1000),
                Some(1001),
            ))
            .unwrap();
        first
            .inode()
            .truncate_storage(987, Some(&AccessIdentity::root()))
            .unwrap();
        first.inode().set_times(Some(5), Some(6)).unwrap();
        let metadata = second.inode().metadata().unwrap();
        assert_eq!(
            (
                metadata.mode,
                metadata.uid,
                metadata.gid,
                metadata.size,
                metadata.atime,
                metadata.mtime
            ),
            (0o100600, 1000, 1001, 987, 5, 6)
        );
        assert_eq!(
            second.inode().namespace_reference().unwrap().id(),
            namespace.id()
        );
        drop(first);
        let third = namespace_opened(NamespaceReference::new(namespace)).unwrap();
        assert!(Arc::ptr_eq(&second.inode(), &third.inode()));
        assert_eq!(third.inode().size(), 987);
    }
    #[test]
    fn nsfs_bind_dentry_keeps_identity_after_all_fds_close_then_releases_namespace() {
        use crate::namespace::{NamespaceAuthority, NamespaceReference, UserNamespace};
        let parent = crate::namespace::boot();
        let authority = NamespaceAuthority {
            namespace: parent,
            effective_uid: 0,
            effective_gid: 0,
            effective_capabilities: syscall_abi::capability::VALID_MASK,
        };
        let child = UserNamespace::child(
            &authority,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX,
            },
        )
        .unwrap();
        let weak = Arc::downgrade(&child);
        let first = namespace_opened(NamespaceReference::new(child.clone())).unwrap();
        // VFS bind root的跨边界事实是同一actual dentry强pin，不保留OpenedFile。
        let bound = first.entry.clone();
        drop(first);
        let second = namespace_opened(NamespaceReference::new(child.clone())).unwrap();
        assert!(Arc::ptr_eq(&bound, &second.entry));
        drop(second);
        drop(child);
        assert!(weak.upgrade().is_some());
        drop(bound);
        assert!(
            weak.upgrade().is_none(),
            "global nsfs empty root must not pin namespace"
        );
    }
    #[test]
    fn nsfs_global_root_has_pseudo_permissions_and_no_namespace_and_files_never_execute() {
        use crate::namespace::{NamespaceAuthority, NamespaceReference, UserNamespace};
        let authority = NamespaceAuthority {
            namespace: crate::namespace::boot(),
            effective_uid: 0,
            effective_gid: 0,
            effective_capabilities: syscall_abi::capability::VALID_MASK,
        };
        let child = UserNamespace::child(
            &authority,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX,
            },
        )
        .unwrap();
        let opened = namespace_opened(NamespaceReference::new(child)).unwrap();
        let root = opened.mount.filesystem.root_inode().unwrap();
        let metadata = root.metadata().unwrap();
        assert_eq!(
            (metadata.mode, metadata.links, metadata.inode),
            (0o040600, 1, 1)
        );
        assert!(root.namespace_reference().is_none());
        let statistics = opened.mount.filesystem.statistics().unwrap();
        assert_eq!(
            (
                opened.mount.filesystem.superblock().type_name,
                statistics.magic,
                statistics.flags
            ),
            ("nsfs", 0x6e73_6673, 0x20)
        );
        opened
            .inode()
            .change_owner_mode(OwnerModeChange::chmod(AccessIdentity::root(), 0o777))
            .unwrap();
        assert!(!opened.inode().is_executable());
        assert_eq!(
            opened.inode().require_access(&AccessIdentity::root(), 1),
            Err(FileSystemError::AccessDenied)
        );
    }
    #[test]
    fn bound_namespace_dentry_survives_all_fd_closes_without_pinning_namespace_in_global_root() {
        use crate::namespace::{NamespaceAuthority, NamespaceReference, UserNamespace};
        let authority = NamespaceAuthority {
            namespace: crate::namespace::boot(),
            effective_uid: 0,
            effective_gid: 0,
            effective_capabilities: syscall_abi::capability::VALID_MASK,
        };
        let namespace = UserNamespace::child(
            &authority,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX,
            },
        )
        .unwrap();
        let weak_namespace = Arc::downgrade(&namespace);
        let first = namespace_opened(NamespaceReference::new(namespace.clone())).unwrap();
        let bound_root = first.entry.clone(); // 实际bind view所持dentry的边界pin。
        let filesystem = first.mount.clone();
        drop(first);
        crate::allocation_failure::fail_next();
        let failed = namespace_opened(NamespaceReference::new(namespace.clone()));
        assert!(matches!(failed, Err(FileSystemError::OutOfMemory)));
        assert!(crate::allocation_failure::was_consumed());
        let reopened = namespace_opened(NamespaceReference::new(namespace.clone())).unwrap();
        assert!(Arc::ptr_eq(&bound_root, &reopened.entry));
        assert!(Arc::ptr_eq(&filesystem, &reopened.mount));
        let root = filesystem.filesystem.root_inode().unwrap();
        let metadata = root.metadata().unwrap();
        assert_eq!(
            (metadata.mode, metadata.links, metadata.atime),
            (0o040600, 1, 11)
        );
        assert!(root.namespace_reference().is_none());
        let statistics = filesystem.filesystem.statistics().unwrap();
        assert_eq!(
            (
                filesystem.filesystem.superblock().type_name,
                statistics.flags
            ),
            ("nsfs", 0x20)
        );
        drop(reopened);
        drop(namespace);
        assert!(weak_namespace.upgrade().is_some());
        drop(bound_root);
        assert!(weak_namespace.upgrade().is_none());
    }

    #[test]
    fn namespace_inode_cannot_become_executable_after_chmod() {
        use crate::namespace::{NamespaceAuthority, NamespaceReference, UserNamespace};
        let authority = NamespaceAuthority {
            namespace: crate::namespace::boot(),
            effective_uid: 0,
            effective_gid: 0,
            effective_capabilities: syscall_abi::capability::VALID_MASK,
        };
        let namespace = UserNamespace::child(
            &authority,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX,
            },
        )
        .unwrap();
        let opened = namespace_opened(NamespaceReference::new(namespace)).unwrap();
        let inode = opened.inode();
        inode
            .change_owner_mode(OwnerModeChange::chmod(AccessIdentity::root(), 0o777))
            .unwrap();
        assert!(!inode.is_executable());
        assert_eq!(
            inode.require_access(&AccessIdentity::root(), 1),
            Err(FileSystemError::AccessDenied)
        );
        assert_eq!(inode.require_access(&AccessIdentity::root(), 4), Ok(()));
    }

    #[test]
    fn nsfs_owner_ioctl_returns_concrete_parent_and_device_has_no_positioned_operations() {
        use crate::namespace::{NamespaceAuthority, NamespaceReference, UserNamespace};
        struct Memory(core::cell::RefCell<Option<NamespaceReference>>);
        impl chardev::UserMemory for Memory {
            fn pid_namespace(&self) -> Arc<crate::namespace::PidNamespace> {
                crate::namespace::initial_pid()
            }
            fn install_namespace(
                &self,
                value: NamespaceReference,
            ) -> Result<isize, chardev::DeviceError> {
                *self.0.borrow_mut() = Some(value);
                Ok(23)
            }
            fn write(&self, _: usize, _: &[u8]) -> Result<(), chardev::UserFault> {
                Ok(())
            }
        }
        let parent = crate::namespace::boot();
        let authority = NamespaceAuthority {
            namespace: parent.clone(),
            effective_uid: 0,
            effective_gid: 0,
            effective_capabilities: syscall_abi::capability::VALID_MASK,
        };
        let child = UserNamespace::child(
            &authority,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX,
            },
        )
        .unwrap();
        let opened = namespace_opened(NamespaceReference::new(child.clone())).unwrap();
        let InodeOpen::Device(file) = opened
            .inode()
            .open_backend(0, &AccessIdentity::root(), &authority)
            .ok()
            .unwrap();
        assert!(file.positioned().is_none());
        let user = Memory(core::cell::RefCell::new(None));
        assert_eq!(
            file.ioctl(&chardev::IoctlCall {
                fd_readable: true,
                request: 0xb701,
                argument: 0,
                user: &user,
                authority: authority.clone()
            }),
            Ok(23)
        );
        let actual = user
            .0
            .borrow_mut()
            .take()
            .expect("related namespace")
            .downcast::<UserNamespace>()
            .unwrap();
        assert!(Arc::ptr_eq(&actual, &parent));
        let confined = NamespaceAuthority {
            namespace: child.clone(),
            ..authority
        };
        assert_eq!(
            file.ioctl(&chardev::IoctlCall {
                fd_readable: true,
                request: 0xb702,
                argument: 0,
                user: &user,
                authority: confined
            }),
            Err(chardev::DeviceError::Errno(1))
        );
        assert!(user.0.borrow().is_none());
        assert_eq!(
            opened.label(),
            format!("user:[{}]", NamespaceReference::new(child).inode()).into_bytes()
        );
    }
}
