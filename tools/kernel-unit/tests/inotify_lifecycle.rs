#![feature(allocator_ext)]
#![allow(dead_code, unused_imports)]
//! 实际 inotify instance/watch/queue/Pipe owner；inode 事实、用户 copy 与调用边界由 fixture 生产。
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../../../kernel/src/id.rs"]
mod id;
#[path = "../src/pipe_fixture.rs"]
mod ipc;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../src/sync.rs"]
mod sync;

#[path = "../../../kernel/src/storage/fs"]
mod fs {
    use alloc::{sync::Arc, vec::Vec};
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum InodeType {
        File,
        Directory,
    }
    // boot watch-cost 的 inode-size 输入边界；此 fixture 验证生命周期，不加载 tmpfs storage。
    mod tmpfs {
        pub(crate) struct TmpInode([u8; 512]);
    }
    pub(crate) struct Metadata {
        pub(crate) inode: u64,
        pub(crate) links: u32,
    }
    pub(crate) trait Inode: Send + Sync {
        fn notification_link_state(&self) -> Option<(u64, u32)> {
            self.metadata()
                .ok()
                .map(|metadata| (metadata.inode, metadata.links))
        }

        fn allows_notifications(&self) -> bool {
            true
        }
        fn filesystem_id(&self) -> usize;
        fn inode_type(&self) -> InodeType;
        fn metadata(&self) -> Result<Metadata, ()>;
    }
    pub(crate) struct OpenedFile {
        inode: Arc<dyn Inode>,
        parent: Arc<dyn Inode>,
        name: Vec<u8>,
        unlinked: bool,
    }
    impl OpenedFile {
        pub(crate) fn inode(&self) -> Arc<dyn Inode> {
            self.inode.clone()
        }
        pub(crate) fn watch_context(&self) -> Option<(Arc<dyn Inode>, Vec<u8>, bool)> {
            Some((self.parent.clone(), self.name.clone(), self.unlinked))
        }
        pub(crate) fn is_unlinked(&self) -> bool {
            self.unlinked
        }
    }
    pub(crate) mod device {
        use super::*;
        #[derive(Debug, PartialEq, Eq)]
        pub(crate) enum DeviceError {
            ReadFault,
            Errno(isize),
        }
        #[derive(Debug)]
        pub(crate) struct UserFault;
        pub(crate) trait UserOutput {
            fn remaining(&self) -> usize;
            fn write(&mut self, bytes: &[u8]) -> Result<(), UserFault>;
        }
        pub(crate) trait UserMemory {
            fn write(&self, address: usize, bytes: &[u8]) -> Result<(), UserFault>;
        }
        pub(crate) struct IoctlCall<'a> {
            pub(crate) request: usize,
            pub(crate) argument: usize,
            pub(crate) user: &'a dyn UserMemory,
        }
        pub(crate) struct DeviceWaitSources(Option<Arc<crate::ipc::Pipe>>);
        impl DeviceWaitSources {
            pub(crate) fn new() -> Self {
                Self(None)
            }
            pub(crate) fn pipe(pipe: Arc<crate::ipc::Pipe>, _: i16) -> Self {
                Self(Some(pipe))
            }
        }
        pub(crate) trait DeviceFile: Send + Sync {
            fn supports_read(&self) -> bool {
                true
            }
            fn supports_write(&self) -> bool {
                true
            }
            fn release(&self) {}
            fn anonymous_name(&self) -> &'static [u8];
            fn read(
                &self,
                output: &mut dyn UserOutput,
                nonblocking: bool,
            ) -> Result<(), DeviceError>;
            fn poll(&self, events: i16) -> i16;
            fn wait_sources(&self, events: i16) -> DeviceWaitSources;
            fn readiness_generation(&self) -> u64;
            fn prepare_wait(&self, events: i16) -> Option<Arc<crate::ipc::Pipe>>;
            fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError>;
            fn inotify(&self) -> Option<&super::inotify::Inotify>;
        }
        pub(crate) fn wait_ready(
            _: &dyn DeviceFile,
            _: i16,
            nonblocking: bool,
        ) -> Result<(), DeviceError> {
            assert!(
                nonblocking,
                "blocking scheduler routing is exercised in guest"
            );
            Err(DeviceError::Errno(syscall_abi::errno::EAGAIN))
        }
    }
    mod inotify;
    use device::{DeviceError, DeviceFile, UserFault, UserOutput};
    use inotify::{InitError, Inotify, WatchError};
    fn new() -> Result<Arc<Inotify>, InitError> {
        Inotify::new(crate::namespace::boot(), 0)
    }

    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    struct InputInode(u64);
    impl Inode for InputInode {
        fn filesystem_id(&self) -> usize {
            78
        }
        fn inode_type(&self) -> InodeType {
            InodeType::File
        }
        fn metadata(&self) -> Result<Metadata, ()> {
            Ok(Metadata {
                inode: self.0,
                links: 1,
            })
        }
    }
    struct Output {
        bytes: Vec<u8>,
        capacity: usize,
        fault: bool,
    }
    impl UserOutput for Output {
        fn remaining(&self) -> usize {
            self.capacity - self.bytes.len()
        }
        fn write(&mut self, bytes: &[u8]) -> Result<(), UserFault> {
            if self.fault {
                return Err(UserFault);
            }
            self.bytes.extend_from_slice(bytes);
            Ok(())
        }
    }
    fn read(instance: &Inotify) -> Vec<(i32, u32)> {
        let mut output = Output {
            bytes: Vec::new(),
            capacity: 4096,
            fault: false,
        };
        instance.read(&mut output, true).unwrap();
        output
            .bytes
            .as_chunks::<16>()
            .0
            .iter()
            .map(|record| {
                (
                    i32::from_ne_bytes(record[0..4].try_into().unwrap()),
                    u32::from_ne_bytes(record[4..8].try_into().unwrap()),
                )
            })
            .collect()
    }
    #[test]
    fn actual_release_returns_admission_while_all_backend_observers_survive() {
        let _serial = SERIAL.lock().unwrap();
        let originals: Vec<_> = (0..128).map(|_| new().unwrap()).collect();
        assert!(matches!(new(), Err(InitError::TooManyInstances)));
        for instance in &originals {
            instance.release();
        }
        let replacements: Vec<_> = (0..128).map(|_| new().unwrap()).collect();
        for instance in &originals {
            instance.release();
            assert_eq!(instance.add_watch((78, 1), 2), Err(WatchError::Closed));
            assert_ne!(instance.poll(1) & 16, 0);
        }
        drop(originals);
        assert!(matches!(new(), Err(InitError::TooManyInstances)));
        drop(replacements);
    }
    #[test]
    fn failed_construction_never_double_refunds_instance_quota() {
        let _serial = SERIAL.lock().unwrap();
        crate::namespace::boot();
        for stage in 0..20 {
            crate::allocation_failure::fail_after(stage);
            let result = new();
            let consumed = crate::allocation_failure::was_consumed();
            if consumed {
                assert!(matches!(
                    result,
                    Err(InitError::OutOfMemory | InitError::TooManyInstances)
                ));
            }
            drop(result);
        }
        let instances: Vec<_> = (0..128).map(|_| new().unwrap()).collect();
        assert!(matches!(new(), Err(InitError::TooManyInstances)));
        drop(instances);
    }
    #[test]
    fn watch_storage_failure_is_atomic_and_existing_update_allocates_nothing() {
        let _serial = SERIAL.lock().unwrap();
        let instance = new().unwrap();
        let wd = instance.add_watch((78, 1), 2).unwrap();
        crate::allocation_failure::fail_next();
        let updated = instance.add_watch((78, 1), 4);
        let consumed = crate::allocation_failure::was_consumed();
        assert_eq!(updated, Ok(wd));
        assert!(!consumed);
        for stage in 0..6 {
            crate::allocation_failure::fail_after(stage);
            let result = instance.add_watch((78, 2), 2);
            let consumed = crate::allocation_failure::was_consumed();
            assert!(consumed);
            assert_eq!(
                result,
                Err(if stage == 0 {
                    WatchError::TooManyWatches
                } else {
                    WatchError::OutOfMemory
                })
            );
        }
        assert_eq!(
            instance.add_watch((78, 1), 2 | 0x1000_0000),
            Err(WatchError::AlreadyExists)
        );
        assert_eq!(
            instance.add_watch((78, 1), 2 | 0x3000_0000),
            Err(WatchError::InvalidMask)
        );
        assert_eq!(instance.add_watch((78, 2), 2).unwrap(), wd + 1);
    }
    #[test]
    fn oneshot_retires_exact_mark_with_ordered_ignored_and_no_late_delivery() {
        let _serial = SERIAL.lock().unwrap();
        let instance = new().unwrap();
        let wd = instance.add_watch((78, 1), 2 | 0x8000_0000).unwrap();
        inotify::notify_self(&InputInode(1), 2);
        inotify::notify_self(&InputInode(1), 2);
        assert_eq!(read(&instance), vec![(wd, 2), (wd, 0x8000)]);
        assert!(!instance.remove_watch(wd));
        let next = instance.add_watch((78, 1), 2).unwrap();
        inotify::notify_self(&InputInode(1), 2);
        assert_eq!(read(&instance), vec![(next, 2)]);
    }
    #[test]
    fn copy_fault_consumes_only_current_record_and_read_requires_no_staging_allocation() {
        let _serial = SERIAL.lock().unwrap();
        let instance = new().unwrap();
        let wd = instance.add_watch((78, 1), 2 | 4).unwrap();
        inotify::notify_self(&InputInode(1), 2);
        inotify::notify_self(&InputInode(1), 4);
        let mut fault = Output {
            bytes: Vec::new(),
            capacity: 16,
            fault: true,
        };
        assert_eq!(instance.read(&mut fault, true), Err(DeviceError::ReadFault));
        let mut output = Output {
            bytes: Vec::with_capacity(16),
            capacity: 16,
            fault: false,
        };
        crate::allocation_failure::fail_next();
        let result = instance.read(&mut output, true);
        let consumed = crate::allocation_failure::was_consumed();
        assert!(!consumed, "record read must not allocate staging storage");
        assert_eq!(result, Ok(()));
        assert_eq!(&output.bytes[0..4], &wd.to_ne_bytes());
        assert_eq!(&output.bytes[4..8], &4u32.to_ne_bytes());
        assert_eq!(instance.poll(1), 0);
    }
    #[test]
    fn excl_unlink_filters_actual_opened_dentry_while_normal_watch_keeps_events() {
        let _serial = SERIAL.lock().unwrap();
        let excluded = new().unwrap();
        let normal = new().unwrap();
        let wd = excluded.add_watch((78, 1), 2 | 0x0400_0000).unwrap();
        let normal_wd = normal.add_watch((78, 1), 2).unwrap();
        let opened = Arc::new(OpenedFile {
            inode: Arc::new(InputInode(1)),
            parent: Arc::new(InputInode(2)),
            name: b"unlinked".to_vec(),
            unlinked: true,
        });
        inotify::notify_opened(&opened, 2);
        assert_eq!(excluded.poll(1), 0);
        assert_eq!(read(&normal), vec![(normal_wd, 2)]);
        inotify::notify_self(&InputInode(1), 2);
        assert_eq!(read(&excluded), vec![(wd, 2)]);
    }
    #[test]
    fn instance_limits_are_per_real_uid_and_not_one_global_pool() {
        let _serial = SERIAL.lock().unwrap();
        let namespace = crate::namespace::boot();
        let first: Vec<_> = (0..128)
            .map(|_| Inotify::new(namespace.clone(), 1000).unwrap())
            .collect();
        assert!(matches!(
            Inotify::new(namespace.clone(), 1000),
            Err(InitError::TooManyInstances)
        ));
        let second: Vec<_> = (0..128)
            .map(|_| Inotify::new(namespace.clone(), 2000).unwrap())
            .collect();
        assert!(matches!(
            Inotify::new(namespace.clone(), 2000),
            Err(InitError::TooManyInstances)
        ));
        first[0].release();
        let replacement = Inotify::new(namespace, 1000).unwrap();
        drop((first, second, replacement));
    }
    #[test]
    fn watches_share_instance_creator_quota_and_ancestor_owner_limit() {
        use crate::namespace::{NamespaceAuthority, UserNamespace, UserResource};
        let _serial = SERIAL.lock().unwrap();
        let initial = crate::namespace::boot();
        let root = crate::namespace::root_authority();
        let limit = inotify::InotifyLimit::Watches.value();
        inotify::InotifyLimit::Watches.set(2, &root).unwrap();
        assert_eq!(initial.resource_limit(UserResource::InotifyWatches), 2);
        let creator = NamespaceAuthority {
            namespace: initial,
            effective_uid: 1000,
            effective_gid: 1000,
            effective_capabilities: 0,
        };
        let child = UserNamespace::child(
            &creator,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX,
            },
        )
        .unwrap();
        child
            .set_resource_limit(UserResource::InotifyWatches, 1, &creator)
            .unwrap();
        let first = Inotify::new(child.clone(), 2000).unwrap();
        let same_user = Inotify::new(child.clone(), 2000).unwrap();
        let other_user = Inotify::new(child, 3000).unwrap();
        let wd = first.add_watch((78, 1), 2).unwrap();
        assert_eq!(
            same_user.add_watch((78, 2), 2),
            Err(WatchError::TooManyWatches)
        );
        assert_eq!(first.add_watch((78, 1), 4), Ok(wd));
        other_user.add_watch((78, 3), 2).unwrap();
        // 新 child 自己有余量，但同一个 creator kuid 的 initial ancestor 已收满两条。
        let sibling = UserNamespace::child(
            &creator,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX,
            },
        )
        .unwrap();
        let third = Inotify::new(sibling, 4000).unwrap();
        assert_eq!(third.add_watch((78, 4), 2), Err(WatchError::TooManyWatches));
        assert!(first.remove_watch(wd));
        third.add_watch((78, 4), 2).unwrap();
        third.release();
        same_user.add_watch((78, 2), 2).unwrap();
        drop((first, same_user, other_user, third));
        inotify::InotifyLimit::Watches.set(limit, &root).unwrap();
    }
    #[test]
    fn queued_event_sysctl_changes_future_instances_and_accepts_zero() {
        let _serial = SERIAL.lock().unwrap();
        crate::namespace::boot();
        let root = crate::namespace::root_authority();
        let limit = inotify::InotifyLimit::QueuedEvents.value();
        let existing = new().unwrap();
        let wd = existing.add_watch((78, 1), 2).unwrap();
        inotify::InotifyLimit::QueuedEvents.set(0, &root).unwrap();
        let future = new().unwrap();
        future.add_watch((78, 1), 2).unwrap();
        inotify::notify_self(&InputInode(1), 2);
        assert_eq!(read(&existing), vec![(wd, 2)]);
        assert_eq!(read(&future), vec![(-1, 0x4000)]);
        let unprivileged = crate::namespace::NamespaceAuthority {
            effective_uid: 1000,
            ..root.clone()
        };
        assert_eq!(
            inotify::InotifyLimit::QueuedEvents.set(1, &unprivileged),
            Err(crate::namespace::NamespaceError::Permission)
        );
        inotify::InotifyLimit::QueuedEvents
            .set(limit, &root)
            .unwrap();
    }

    #[test]
    fn oneshot_allocation_failure_reports_overflow_without_retiring_the_watch() {
        let _serial = SERIAL.lock().unwrap();
        let instance = new().unwrap();
        let wd = instance.add_watch((78, 1), 2 | 0x8000_0000).unwrap();
        crate::allocation_failure::fail_next();
        inotify::notify_self(&InputInode(1), 2);
        let consumed = crate::allocation_failure::was_consumed();
        assert!(consumed);
        assert_eq!(read(&instance), vec![(-1, 0x4000)]);
        inotify::notify_self(&InputInode(1), 2);
        assert_eq!(instance.poll(0x40), 0x40);
        assert_eq!(read(&instance), vec![(wd, 2), (wd, 0x8000)]);
    }
    #[test]
    fn dentry_reclamation_checks_actual_last_hard_link_before_retiring_inode_marks() {
        struct Removed;
        impl Inode for Removed {
            fn filesystem_id(&self) -> usize {
                78
            }
            fn inode_type(&self) -> InodeType {
                InodeType::File
            }
            fn metadata(&self) -> Result<Metadata, ()> {
                Ok(Metadata { inode: 1, links: 0 })
            }
        }
        let _serial = SERIAL.lock().unwrap();
        let instance = new().unwrap();
        let wd = instance.add_watch((78, 1), 0x400).unwrap();
        inotify::notify_dentry_reclaimed(&InputInode(1));
        assert_eq!(instance.poll(1), 0);
        inotify::notify_dentry_reclaimed(&Removed);
        assert_eq!(read(&instance), vec![(wd, 0x400), (wd, 0x8000)]);
        inotify::notify_dentry_reclaimed(&Removed);
        assert_eq!(instance.poll(1), 0);
    }
    #[test]
    fn mutable_inode_notifications_do_not_require_full_metadata() {
        struct MinimalFacts(core::sync::atomic::AtomicU32);
        impl Inode for MinimalFacts {
            fn filesystem_id(&self) -> usize {
                78
            }
            fn inode_type(&self) -> InodeType {
                InodeType::Directory
            }
            fn notification_link_state(&self) -> Option<(u64, u32)> {
                Some((19, self.0.load(core::sync::atomic::Ordering::Relaxed)))
            }
            fn metadata(&self) -> Result<Metadata, ()> {
                panic!("notification must not allocate or inspect full metadata")
            }
        }
        let _serial = SERIAL.lock().unwrap();
        let inode = MinimalFacts(core::sync::atomic::AtomicU32::new(1));
        let instance = new().unwrap();
        let wd = instance.add_watch((78, 19), 2 | 0x400).unwrap();
        inotify::notify_self(&inode, 2);
        assert_eq!(read(&instance), vec![(wd, 2 | 0x4000_0000)]);
        inotify::notify_dentry_reclaimed(&inode);
        assert_eq!(instance.poll(1), 0);
        inode.0.store(0, core::sync::atomic::Ordering::Relaxed);
        inotify::notify_dentry_reclaimed(&inode);
        assert_eq!(read(&instance), vec![(wd, 0x400), (wd, 0x8000)]);
        inotify::notify_dentry_reclaimed(&inode);
        assert_eq!(instance.poll(1), 0);
    }
}
