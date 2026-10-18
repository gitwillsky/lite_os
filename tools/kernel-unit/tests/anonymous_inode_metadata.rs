#![feature(allocator_ext)]
#![allow(dead_code)]
//! Actual pseudo inode scalar owner and permission policy; filesystem ID and realtime clock are seams.
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
mod timer {
    pub(crate) fn get_realtime_ns() -> u64 {
        5_000_000_000
    }
}
#[path = "../../../kernel/src/storage/fs"]
mod fs {
    use alloc::sync::Arc;
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum FileSystemError {
        AccessDenied,
        InvalidOperation,
        PermissionDenied,
        OutOfMemory,
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum InodeType {
        File,
        Directory,
        Fifo,
        Socket,
    }
    #[derive(Clone, Copy)]
    pub(crate) struct CreateMetadata {
        pub(crate) mode: u32,
        pub(crate) uid: u32,
        pub(crate) gid: u32,
    }
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub(crate) struct FineTimes {
        pub(crate) atime_nsec: u32,
        pub(crate) mtime_nsec: u32,
        pub(crate) ctime_nsec: u32,
        pub(crate) birth: Option<(u64, u32)>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct InodeMetadata {
        pub(crate) filesystem: u64,
        pub(crate) inode: u64,
        pub(crate) kind: InodeType,
        pub(crate) mode: u32,
        pub(crate) uid: u32,
        pub(crate) gid: u32,
        pub(crate) links: u32,
        pub(crate) size: u64,
        pub(crate) blocks: u64,
        pub(crate) block_size: u32,
        pub(crate) atime: u64,
        pub(crate) mtime: u64,
        pub(crate) ctime: u64,
        pub(crate) fine_times: FineTimes,
        pub(crate) device: Option<()>,
    }
    mod inode_times;
    mod permission;
    pub(crate) use permission::{AccessIdentity, OwnerModeChange};
    mod anonymous {
        #[derive(Clone, Copy)]
        pub(super) struct PseudoFileSystem;
        impl PseudoFileSystem {
            pub(super) fn id(self) -> usize {
                42
            }
        }
        mod metadata;
        pub(super) use metadata::Metadata;
    }
    fn inode(kind: InodeType, mode: u32) -> anonymous::Metadata {
        crate::namespace::boot();
        anonymous::Metadata::new(
            anonymous::PseudoFileSystem,
            17,
            kind,
            CreateMetadata {
                mode,
                uid: 123,
                gid: 456,
            },
            0,
            inode_times::TimeOwner::new(),
        )
    }
    #[test]
    fn actual_pseudo_inode_preserves_creator_type_and_inode_facts() {
        for (kind, mode) in [
            (InodeType::File, 0o100777),
            (InodeType::Fifo, 0o010600),
            (InodeType::Socket, 0o140777),
        ] {
            let inode = inode(kind, mode);
            let metadata = inode.snapshot(12, 8);
            assert_eq!(
                (
                    metadata.filesystem,
                    metadata.inode,
                    metadata.kind,
                    metadata.mode
                ),
                (42, 17, kind, mode)
            );
            assert_eq!(
                (
                    metadata.uid,
                    metadata.gid,
                    metadata.links,
                    metadata.size,
                    metadata.blocks
                ),
                (123, 456, 0, 12, 8)
            );
            assert_eq!((metadata.atime, metadata.mtime, metadata.ctime), (5, 5, 5));
        }
    }
    #[test]
    fn actual_pseudo_attribute_failure_keeps_the_entire_tuple() {
        let inode = inode(InodeType::File, 0o100777);
        let before = inode.snapshot(0, 0);
        let intruder = AccessIdentity::new(999, 999, None, crate::namespace::boot(), 0, 999);
        assert_eq!(
            inode.change(&OwnerModeChange::chmod(intruder, 0o600)),
            Err(FileSystemError::PermissionDenied)
        );
        assert_eq!(inode.snapshot(0, 0), before);
    }
    #[test]
    fn actual_pseudo_chmod_chown_use_one_live_tuple() {
        let inode = inode(InodeType::File, 0o100777);
        inode
            .change(&OwnerModeChange::chmod(AccessIdentity::root(), 0o6777))
            .unwrap();
        assert_eq!(inode.snapshot(0, 0).mode, 0o106777);
        inode
            .change(&OwnerModeChange::chown(
                AccessIdentity::root(),
                Some(789),
                Some(987),
            ))
            .unwrap();
        let metadata = inode.snapshot(0, 0);
        assert_eq!(
            (metadata.uid, metadata.gid, metadata.mode),
            (789, 987, 0o100777)
        );
    }
    #[test]
    fn actual_pseudo_attribute_commit_does_not_allocate_or_consume_group_owner() {
        let inode = inode(InodeType::File, 0o100777);
        let groups = Arc::new(vec![456]);
        let change = OwnerModeChange::chmod(
            AccessIdentity::new(
                123,
                999,
                Some(groups.clone()),
                crate::namespace::boot(),
                0,
                123,
            ),
            0o2660,
        );
        let before = Arc::strong_count(&groups);
        crate::allocation_failure::fail_next();
        inode.change(&change).unwrap();
        assert!(!crate::allocation_failure::was_consumed());
        assert_eq!(Arc::strong_count(&groups), before);
        assert_eq!(inode.snapshot(0, 0).mode, 0o102660);
        drop(change);
        assert_eq!(Arc::strong_count(&groups), before - 1);
    }
    #[test]
    fn actual_pseudo_times_accept_past_values_and_content_mtime_is_not_max_clamped() {
        let inode = inode(InodeType::File, 0o100777);
        inode.set_times(Some(2), Some(1));
        let metadata = inode.snapshot(0, 0);
        assert_eq!((metadata.atime, metadata.mtime, metadata.ctime), (2, 1, 5));
        inode.set_times(None, Some(8));
        let future = inode.snapshot(0, 0);
        assert_eq!((future.atime, future.mtime, future.ctime), (2, 8, 5));
        inode.set_times(Some(3), None);
        let atime_only = inode.snapshot(0, 0);
        assert_eq!(
            (atime_only.atime, atime_only.mtime, atime_only.ctime),
            (3, 8, 5)
        );
    }
    #[test]
    #[ignore = "diagnostic host benchmark; run alone in release"]
    fn actual_shared_inode_time_owner_cost() {
        use std::{hint::black_box, time::Instant};
        let times = inode_times::InodeTimes::new().unwrap();
        let metadata = anonymous::Metadata::new(
            anonymous::PseudoFileSystem,
            17,
            InodeType::File,
            CreateMetadata {
                mode: 0o100777,
                uid: 123,
                gid: 456,
            },
            0,
            inode_times::TimeOwner::Memory(times.clone()),
        );
        for operation in [
            "content time touch",
            "content time snapshot",
            "metadata with shared times",
        ] {
            let run = || match operation {
                "content time touch" => times.touch(),
                "content time snapshot" => {
                    black_box(times.snapshot());
                }
                _ => {
                    black_box(metadata.snapshot(12, 8));
                }
            };
            for _ in 0..100_000 {
                run();
            }
            let mut samples = Vec::new();
            for _ in 0..5 {
                let start = Instant::now();
                for _ in 0..1_000_000 {
                    run();
                }
                samples.push(start.elapsed().as_nanos() as f64 / 1_000_000.0);
            }
            samples.sort_by(f64::total_cmp);
            println!("{operation}: median {:.2} ns", samples[2]);
        }
    }
}
