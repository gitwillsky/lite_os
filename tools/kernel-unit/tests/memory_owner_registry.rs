#![feature(allocator_ext)]
//! 用可重入/可失败的 host allocator 直接执行生产注册事务，验证 OOM 不在 registry 锁内回收。

extern crate alloc;

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

thread_local! {
    static REENTER_RECLAIM: Cell<bool> = const { Cell::new(false) };
    static FAIL_ALLOCATION: Cell<bool> = const { Cell::new(false) };
    static REENTRIES: Cell<usize> = const { Cell::new(0) };
}

struct ReclaimingAllocator;

// SAFETY: 正常 allocation/deallocation 完整委托 System；故障注入只拒绝当前线程的一次 allocation，
// 可重入探针先清除自身，防止 probe 内的非目标 allocation 再次递归。
unsafe impl GlobalAlloc for ReclaimingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if REENTER_RECLAIM
            .try_with(|flag| flag.replace(false))
            .unwrap_or(false)
        {
            REENTRIES.with(|count| count.set(count.get() + 1));
            let _ = memory::shared_file::reclaim_pages(1);
        }
        if FAIL_ALLOCATION
            .try_with(|flag| flag.replace(false))
            .unwrap_or(false)
        {
            return core::ptr::null_mut();
        }
        // SAFETY: layout 原样交给 System，不改变返回 allocation 的所有权。
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: 除显式失败外的 allocation 均来自同一 System 和 layout。
        unsafe { System.dealloc(pointer, layout) };
    }
}

#[global_allocator]
static ALLOCATOR: ReclaimingAllocator = ReclaimingAllocator;

#[path = "../../../kernel/src/fallible_tree/mod.rs"]
#[allow(dead_code)]
mod fallible_tree;

mod sync {
    pub(crate) struct TaskMutexWaitPreparation;
}

// 不执行页内容或 MMU 操作；这里只提供生产 shared_file 的独立 registry 所需类型。
#[path = "../../../kernel/src/memory"]
#[allow(dead_code)]
mod memory {
    pub(crate) mod config {
        pub(crate) const PAGE_SIZE: usize = 4096;
    }

    pub(crate) mod address {
        #[derive(Debug, Clone, Copy)]
        pub(crate) struct PhysicalPageNumber(usize);

        impl PhysicalPageNumber {
            pub(crate) fn as_page_ptr(self) -> *const u8 {
                self.0 as *const u8
            }
            pub(crate) fn as_page_mut_ptr(self) -> *mut u8 {
                self.0 as *mut u8
            }
        }
    }

    pub(crate) mod frame_allocator {
        #[derive(Debug)]
        pub(crate) struct FrameTracker {
            pub(crate) ppn: super::address::PhysicalPageNumber,
            bytes: [u8; 4096],
        }

        impl FrameTracker {
            pub(crate) fn bytes(&self) -> &[u8] {
                &self.bytes
            }
            pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
                &mut self.bytes
            }
        }

        pub(crate) fn alloc_copy(_: &[u8]) -> Option<FrameTracker> {
            panic!("frame copy outside registry fixture")
        }
        pub(crate) fn alloc() -> Option<FrameTracker> {
            None
        }
    }

    // The target is the real mapping registry, not page content. These opaque seam shapes
    // deliberately fail if the fixture attempts physical access or PageOp.
    pub(crate) mod swappable_page {
        pub(crate) struct SwappablePage;
        pub(crate) struct PagePin<'a>(&'a SwappablePage);
        impl PagePin<'_> {
            pub(crate) fn ppn(&self) -> super::address::PhysicalPageNumber {
                panic!("page pin outside registry fixture")
            }
            pub(crate) fn read(&self, _: usize, _: &mut [u8]) {
                panic!("page bytes outside registry fixture")
            }
        }
    }
    pub(crate) mod shared_file;
    mod vmstat;

    pub(crate) fn background_reclaim() {
        let _ = shared_file::reclaim_background_pages(1);
    }
}

struct Reclaimer(AtomicUsize);

impl memory::shared_file::MemoryReclaimer for Reclaimer {
    fn reclaim_pages(
        &self,
        _: memory::shared_file::ReclaimRequest,
    ) -> memory::shared_file::ReclaimResult {
        self.0.fetch_add(1, Ordering::Relaxed);
        memory::shared_file::ReclaimResult::new(0, 0)
    }
}

struct Mapping(AtomicUsize);

impl memory::shared_file::MemoryMappingOwner for Mapping {
    fn revoke_swappable_page(
        &self,
        _: &memory::swappable_page::SwappablePage,
        _: memory::address::PhysicalPageNumber,
    ) -> Option<usize> {
        Some(0)
    }
    fn revoke_shared_file_writers(
        &self,
        _: memory::shared_file::SharedFileId,
        _: &mut sync::TaskMutexWaitPreparation,
    ) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn restore_swapped_pages(
        &self,
        _: memory::shared_file::SharedFileId,
        _: &mut sync::TaskMutexWaitPreparation,
    ) -> Result<(), memory::shared_file::SharedFileError> {
        Ok(())
    }
    fn invalidate_cached_file(
        &self,
        _: memory::shared_file::SharedFileId,
        _: u64,
        _: u64,
        _: &mut sync::TaskMutexWaitPreparation,
    ) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn invalidate_shared_file(
        &self,
        _: memory::shared_file::SharedFileId,
        _: u64,
        _: &mut sync::TaskMutexWaitPreparation,
    ) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn registry_preparation_can_reenter_direct_reclaim() {
    // 原实现会自锁；child watchdog 把真实死锁裁决为失败，避免测试本身永久挂起。
    if std::env::var_os("LITEOS_REGISTRY_CHILD").is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "registry_preparation_can_reenter_direct_reclaim",
                "--nocapture",
            ])
            .env("LITEOS_REGISTRY_CHILD", "1")
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "registry preparation child failed");
                return;
            }
            if started.elapsed() > std::time::Duration::from_secs(10) {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("registration allocation reentered a locked reclaim registry");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    let reclaimer = Arc::new(Reclaimer(AtomicUsize::new(0)));
    let mapping = Arc::new(Mapping(AtomicUsize::new(0)));
    REENTER_RECLAIM.set(true);
    memory::shared_file::register_memory_reclaimer(reclaimer.clone()).unwrap();
    assert_eq!(REENTRIES.get(), 1);
    REENTER_RECLAIM.set(true);
    memory::shared_file::register_memory_mapping_owner(mapping.clone()).unwrap();
    assert_eq!(REENTRIES.get(), 2);
    assert_eq!(reclaimer.0.load(Ordering::Relaxed), 1);

    let another = Arc::new(Reclaimer(AtomicUsize::new(0)));
    FAIL_ALLOCATION.set(true);
    assert!(matches!(
        memory::shared_file::register_memory_reclaimer(another.clone()),
        Err(memory::shared_file::SharedFileError::OutOfMemory)
    ));
    let _ = memory::shared_file::reclaim_pages(1);
    assert_eq!(another.0.load(Ordering::Relaxed), 0);

    drop(reclaimer);
    FAIL_ALLOCATION.set(true);
    memory::shared_file::register_memory_reclaimer(another.clone()).unwrap();
    assert!(
        FAIL_ALLOCATION.replace(false),
        "tombstone reuse must not allocate"
    );
    let _ = memory::shared_file::reclaim_pages(1);
    assert_eq!(another.0.load(Ordering::Relaxed), 1);
    let before = memory::shared_file::reclaim_statistics();
    memory::background_reclaim();
    let after = memory::shared_file::reclaim_statistics();
    assert_eq!(after.direct.attempts, before.direct.attempts);
    assert_eq!(after.background.attempts, before.background.attempts + 1);
    assert_eq!(another.0.load(Ordering::Relaxed), 2);
    let another_mapping = Arc::new(Mapping(AtomicUsize::new(0)));
    FAIL_ALLOCATION.set(true);
    assert!(matches!(
        memory::shared_file::register_memory_mapping_owner(another_mapping.clone()),
        Err(memory::shared_file::SharedFileError::OutOfMemory)
    ));
    memory::shared_file::invalidate_shared_file(
        memory::shared_file::SharedFileId {
            filesystem: 1,
            inode: 1,
        },
        0,
        &mut sync::TaskMutexWaitPreparation,
    );
    assert_eq!(mapping.0.load(Ordering::Relaxed), 1);
    assert_eq!(another_mapping.0.load(Ordering::Relaxed), 0);
    drop(mapping);
    FAIL_ALLOCATION.set(true);
    memory::shared_file::register_memory_mapping_owner(another_mapping.clone()).unwrap();
    assert!(
        FAIL_ALLOCATION.replace(false),
        "mapping tombstone reuse must not allocate"
    );
    memory::shared_file::invalidate_shared_file(
        memory::shared_file::SharedFileId {
            filesystem: 1,
            inode: 1,
        },
        0,
        &mut sync::TaskMutexWaitPreparation,
    );
    assert_eq!(another_mapping.0.load(Ordering::Relaxed), 1);
}
