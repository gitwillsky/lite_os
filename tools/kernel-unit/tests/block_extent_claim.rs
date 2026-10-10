#![feature(allocator_ext)]
//! 执行生产 bdev claim owner，验证物理别名的排他性、并发领取与 OOM 原子发布。
#![allow(dead_code)]

extern crate alloc;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
thread_local! { static FAIL_NEXT_ALLOCATION: Cell<bool> = const { Cell::new(false) }; }
struct FaultAllocator;
// SAFETY: 只有线程局部的一次故障返回 null；所有成功内存原样委托 System。
unsafe impl GlobalAlloc for FaultAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if FAIL_NEXT_ALLOCATION
            .try_with(|flag| flag.replace(false))
            .unwrap_or(false)
        {
            return core::ptr::null_mut();
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: FaultAllocator = FaultAllocator;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../../../kernel/src/storage/fs"]
mod fs {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FileSystemError {
        OutOfMemory,
        InvalidOperation,
        AlreadyExists,
        Busy,
    }
    mod block_claim;
    use block_claim::BlockClaim;

    #[test]
    fn whole_and_overlapping_partition_claims_share_one_owner() {
        let whole = BlockClaim::whole(1, 1000).unwrap();
        let left = whole.partition(2, 100, 300).unwrap();
        let overlap = whole.partition(3, 350, 100).unwrap();
        let right = whole.partition(4, 450, 300).unwrap();
        left.begin_mount().unwrap();
        assert!(whole.bypass_cache());
        assert!(overlap.bypass_cache());
        assert!(!right.bypass_cache());
        assert_eq!(whole.begin_mount(), Err(FileSystemError::Busy));
        assert_eq!(whole.begin_writer(), Err(FileSystemError::Busy));
        assert_eq!(overlap.begin_mount(), Err(FileSystemError::Busy));
        assert_eq!(overlap.begin_writer(), Err(FileSystemError::Busy));
        right.begin_mount().unwrap();
        left.end_mount();
        right.end_mount();
        whole.begin_mount().unwrap();
        assert_eq!(left.begin_writer(), Err(FileSystemError::Busy));
        assert_eq!(right.begin_mount(), Err(FileSystemError::Busy));
        whole.end_mount();
    }

    #[test]
    fn multiple_ofds_on_one_bdev_keep_all_aliases_excluded_until_last_drop() {
        let whole = BlockClaim::whole(1, 1000).unwrap();
        let partition = whole.partition(2, 100, 300).unwrap();
        partition.begin_writer().unwrap();
        assert!(whole.bypass_cache());
        assert!(!partition.bypass_cache());
        partition.begin_writer().unwrap();
        assert_eq!(whole.begin_writer(), Err(FileSystemError::Busy));
        assert_eq!(partition.begin_mount(), Err(FileSystemError::Busy));
        partition.end_writer();
        assert_eq!(whole.begin_mount(), Err(FileSystemError::Busy));
        partition.end_writer();
        whole.begin_writer().unwrap();
        assert!(partition.bypass_cache());
        assert!(!whole.bypass_cache());
        assert_eq!(partition.begin_mount(), Err(FileSystemError::Busy));
        assert_eq!(partition.begin_writer(), Err(FileSystemError::Busy));
        whole.end_writer();
    }

    #[test]
    fn overlapping_alias_iteration_uses_physical_offsets_including_nested_views() {
        let whole = BlockClaim::whole(50, 1000).unwrap();
        let parent = whole.partition(40, 100, 300).unwrap();
        let inner = parent.partition(30, 250, 50).unwrap();
        let touch = whole.partition(20, 400, 200).unwrap();
        let mut after = None;
        let mut aliases = Vec::new();
        while let Some(key) = inner.next_alias(after) {
            aliases.push(key);
            after = Some(key);
        }
        assert_eq!(aliases, [30, 40, 50]);
        inner.begin_mount().unwrap();
        touch.begin_mount().unwrap();
        inner.end_mount();
        touch.end_mount();
        assert!(matches!(
            parent.partition(60, 300, 1),
            Err(FileSystemError::InvalidOperation)
        ));
        assert!(matches!(
            parent.partition(60, u64::MAX, 1),
            Err(FileSystemError::InvalidOperation)
        ));
        assert!(matches!(
            parent.partition(60, 0, 0),
            Err(FileSystemError::InvalidOperation)
        ));
    }

    #[test]
    fn failed_or_unpublished_partition_does_not_poison_future_publication() {
        let whole = BlockClaim::whole(1, 1000).unwrap();
        crate::FAIL_NEXT_ALLOCATION.with(|flag| flag.set(true));
        assert!(matches!(
            whole.partition(2, 100, 300),
            Err(FileSystemError::OutOfMemory)
        ));
        assert!(!crate::FAIL_NEXT_ALLOCATION.with(core::cell::Cell::get));
        let unpublished = whole.partition(2, 100, 300).unwrap();
        assert!(matches!(
            whole.partition(2, 100, 300),
            Err(FileSystemError::AlreadyExists)
        ));
        drop(unpublished);
        let published = whole.partition(2, 100, 300).unwrap();
        published.begin_mount().unwrap();
        published.end_mount();
    }

    #[test]
    fn concurrent_alias_mounts_have_exactly_one_winner() {
        use std::sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        };
        let whole = Arc::new(BlockClaim::whole(1, 1000).unwrap());
        let partition = Arc::new(whole.partition(2, 100, 300).unwrap());
        let phase = Arc::new(Barrier::new(2));
        let winners = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for node in [whole.clone(), partition.clone()] {
                let phase = phase.clone();
                let winners = winners.clone();
                scope.spawn(move || {
                    phase.wait();
                    let acquired = node.begin_mount().is_ok();
                    if acquired {
                        winners.fetch_add(1, Ordering::Relaxed);
                    }
                    // winner keeps its claim until both contenders have completed acquisition.
                    phase.wait();
                    if acquired {
                        node.end_mount();
                    }
                });
            }
        });
        assert_eq!(winners.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn swap_uses_the_same_exclusive_physical_extent_owner() {
        let whole = BlockClaim::whole(1, 1000).unwrap();
        let left = whole.partition(2, 0, 400).unwrap();
        let right = whole.partition(3, 600, 400).unwrap();
        left.begin_swap().unwrap();
        assert!(whole.bypass_cache());
        assert!(left.bypass_cache());
        assert!(!right.bypass_cache());
        assert!(matches!(whole.begin_writer(), Err(FileSystemError::Busy)));
        assert!(matches!(left.begin_writer(), Err(FileSystemError::Busy)));
        assert!(matches!(whole.begin_mount(), Err(FileSystemError::Busy)));
        assert!(matches!(left.begin_mount(), Err(FileSystemError::Busy)));
        right.begin_mount().unwrap();
        right.end_mount();
        left.end_swap();
        whole.begin_writer().unwrap();
        assert!(matches!(left.begin_swap(), Err(FileSystemError::Busy)));
        whole.end_writer();
        whole.begin_mount().unwrap();
        assert!(matches!(left.begin_swap(), Err(FileSystemError::Busy)));
        whole.end_mount();
        left.begin_swap().unwrap();
        left.end_swap();
    }
}
