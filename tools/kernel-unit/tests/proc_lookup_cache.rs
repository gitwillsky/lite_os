#![feature(allocator_ext)]
#![allow(dead_code)]
//! 实际 proc child cache、完整 node parent context 和 dirent codec；inode 壳是显式输入 seam。
extern crate alloc;
#[path = "../src/shm_backing_fixture.rs"]
mod memory;
#[path = "../src/socket_core_fixture.rs"]
mod socket;
#[path = "../src/sync.rs"]
mod sync;

#[path = "../src/ipc_namespace_fixture.rs"]
mod ipc;

#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../../../kernel/src/storage/fs"]
mod fs {
    #[path = "inotify/limits.rs"]
    mod inotify_limits;
    pub(crate) use inotify_limits::InotifyLimit;
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum FileSystemError {
        OutOfMemory,
        InvalidOperation,
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum InodeType {
        File,
        Directory,
        SymLink,
        Fifo,
        CharacterDevice,
        BlockDevice,
        Socket,
    }
    pub(crate) enum MountTableKind {
        Mounts,
        Info,
        Stats,
    }
    mod directory;
    pub(crate) use directory::{DirectoryEntry, MAX_GETDENTS_BATCH_BYTES};
    mod procfs {
        use super::{DirectoryEntry, FileSystemError, InodeType, MAX_GETDENTS_BATCH_BYTES};
        use alloc::sync::Arc;
        mod node;
        use node::{ProcNode, ProcTaskDirectory, ProcThreadDirectory};
        mod lookup_cache;
        struct ProcInode {
            key: (ProcNode, u64),
            number: u64,
        }
        #[test]
        fn directory_cache_retains_same_child_and_releases_all_pins_on_parent_retirement() {
            let cache = lookup_cache::ProcLookupCache::new();
            let child = Arc::new(ProcInode {
                key: (ProcNode::ProcessDir(123), 0),
                number: 91,
            });
            let observer = Arc::downgrade(&child);
            cache.remember(&child).unwrap();
            cache.remember(&child).unwrap();
            assert_eq!(Arc::strong_count(&child), 2);
            drop(child);
            assert_eq!(observer.upgrade().unwrap().number, 91);
            drop(cache);
            assert!(observer.upgrade().is_none());
        }
        #[test]
        fn process_churn_evicts_cache_pins_without_retiring_open_alias() {
            let cache = lookup_cache::ProcLookupCache::new();
            let first = Arc::new(ProcInode {
                key: (ProcNode::ProcessDir(10), 0),
                number: 1,
            });
            cache.remember(&first).unwrap();
            let bound = MAX_GETDENTS_BATCH_BYTES / core::mem::size_of::<DirectoryEntry<'static>>();
            let mut second = None;
            for sequence in 2..=bound + 2 {
                let child = Arc::new(ProcInode {
                    key: (ProcNode::ProcessDir(sequence + 10), 0),
                    number: sequence as u64,
                });
                if sequence == 2 {
                    second = Some(Arc::downgrade(&child));
                }
                cache.remember(&child).unwrap();
            }
            assert!(second.unwrap().upgrade().is_none());
            assert_eq!(Arc::strong_count(&first), 1);
            assert_eq!(first.number, 1);
        }
        #[test]
        fn fd_and_namespace_parents_follow_actual_entry_context_even_for_leader_tid() {
            assert_eq!(
                ProcTaskDirectory::Process.parent(12, 12),
                ProcNode::ProcessDir(12)
            );
            assert_eq!(
                ProcTaskDirectory::Thread(ProcThreadDirectory::Group).parent(12, 12),
                ProcNode::ThreadDir(12, 12, ProcThreadDirectory::Group)
            );
            assert_eq!(
                ProcTaskDirectory::Thread(ProcThreadDirectory::Root).parent(12, 13),
                ProcNode::ThreadDir(12, 13, ProcThreadDirectory::Root)
            );
            assert_eq!(ProcThreadDirectory::Root.parent(12), ProcNode::Root);
            assert_eq!(
                ProcThreadDirectory::Group.parent(12),
                ProcNode::ProcessTaskDir(12)
            );
        }
    }
}
