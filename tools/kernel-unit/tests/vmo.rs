//! 执行内核 VMO 的真实 COW 与 pager 完成路径；仅替换物理内存和调度边界。
#![feature(allocator_ext)]

extern crate alloc;

mod id {
    pub(crate) fn next_koid() -> u64 {
        static NEXT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
        NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
    }
}

#[expect(
    dead_code,
    reason = "完整生产 VMO 需要 DMA 与等待错误类型；本 fixture 执行 COW 与已完成的 pager 等待"
)]
mod execution {
    pub(crate) mod sync {
        pub(crate) use std::thread::yield_now as yield_current_task;
        pub(crate) struct IrqMutex<T>(std::sync::Mutex<T>);
        impl<T> IrqMutex<T> {
            pub(crate) const fn new(value: T) -> Self {
                Self(std::sync::Mutex::new(value))
            }
            pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, T> {
                self.0.lock().unwrap()
            }
        }
        pub(crate) struct TaskMutex<T>(std::sync::Mutex<T>);
        pub(crate) type TaskMutexGuard<'a, T> = std::sync::MutexGuard<'a, T>;
        pub(crate) struct TaskMutexWaitPreparation;

        impl TaskMutexWaitPreparation {
            pub(crate) fn prepare() -> Result<Self, ()> {
                Ok(Self)
            }
        }

        impl<T> TaskMutex<T> {
            pub(crate) const fn new(value: T) -> Self {
                Self(std::sync::Mutex::new(value))
            }
            pub(crate) fn try_lock(&self) -> Option<TaskMutexGuard<'_, T>> {
                self.0.try_lock().ok()
            }
            pub(crate) fn lock(&self) -> Result<TaskMutexGuard<'_, T>, ()> {
                Ok(self.0.lock().unwrap())
            }
            pub(crate) fn lock_prepared(
                &self,
                _: &mut TaskMutexWaitPreparation,
            ) -> TaskMutexGuard<'_, T> {
                self.0.lock().unwrap()
            }
        }

        pub(crate) enum WaitResult {
            Woken,
            OutOfMemory,
            TimedOut,
            Interrupted,
        }
        pub(crate) struct WaitQueue;
        impl WaitQueue {
            pub(crate) fn new() -> Self {
                Self
            }
            pub(crate) fn wake_all(&self) {}
            pub(crate) fn wait_until(
                &self,
                mut ready: impl FnMut() -> bool,
                _: Option<u64>,
            ) -> WaitResult {
                assert!(ready(), "host VMO fixture only observes completed waits");
                WaitResult::Woken
            }
        }
    }
}

mod memory {
    pub(crate) const PAGE_SIZE: usize = 4096;
    pub(crate) enum FrameAllocationClass {
        Reclaimable,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct PhysicalPageNumber(usize);
    impl From<usize> for PhysicalPageNumber {
        fn from(value: usize) -> Self {
            Self(value)
        }
    }
    impl PhysicalPageNumber {
        pub(crate) fn as_usize(self) -> usize {
            self.0
        }
        pub(crate) fn as_page_ptr(self) -> *const u8 {
            (self.0 * PAGE_SIZE) as *const u8
        }
        pub(crate) fn as_page_mut_ptr(self) -> *mut u8 {
            (self.0 * PAGE_SIZE) as *mut u8
        }
    }

    pub(crate) struct FrameTracker {
        pub(crate) ppn: PhysicalPageNumber,
        pub(crate) pages: usize,
    }
    impl FrameTracker {
        // SAFETY: fixture 同样只恢复由压缩 Page 唯一持有的完整宿主页。
        pub(crate) unsafe fn from_raw(ppn: PhysicalPageNumber, pages: usize) -> Self {
            Self { ppn, pages }
        }
    }
    impl Drop for FrameTracker {
        fn drop(&mut self) {
            let layout = std::alloc::Layout::from_size_align(self.pages * PAGE_SIZE, PAGE_SIZE)
                .expect("allocated frame layout");
            // SAFETY: 分配与释放使用同一 layout，tracker 是唯一 owner。
            unsafe { std::alloc::dealloc(self.ppn.as_page_mut_ptr(), layout) };
        }
    }
    pub(crate) fn alloc_contiguous(pages: usize, _: FrameAllocationClass) -> Option<FrameTracker> {
        let pages = pages.checked_next_power_of_two()?;
        let layout =
            std::alloc::Layout::from_size_align(pages.checked_mul(PAGE_SIZE)?, PAGE_SIZE).ok()?;
        // SAFETY: 有效非零 layout，由返回的 tracker 唯一拥有。
        let pointer = core::ptr::NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) })?;
        Some(FrameTracker {
            ppn: PhysicalPageNumber(pointer.as_ptr() as usize / PAGE_SIZE),
            pages,
        })
    }
    pub(crate) mod frame_allocator {
        use super::*;
        /// 夹具不模拟水位：回收只由用例直接驱动单个对象。
        pub(crate) fn under_pressure() -> bool {
            false
        }
        pub(crate) fn alloc() -> Option<FrameTracker> {
            alloc_contiguous(1, FrameAllocationClass::Reclaimable)
        }
        pub(crate) fn alloc_copy(bytes: &[u8]) -> Option<FrameTracker> {
            let frame = alloc()?;
            // SAFETY: 新页独占、足够大，与输入页不重叠。
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    frame.ppn.as_page_mut_ptr(),
                    bytes.len(),
                )
            };
            Some(frame)
        }
    }
}

// 保持实际 vm → vmo 作用域；显式目录让 source-view 入口沿用本 fixture 的路径基准。
#[path = "."]
mod vm {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum VmError {
        NoMemory,
        InvalidArgs,
        OutOfRange,
        NotSupported,
        BadState,
        Io,
        Interrupted,
    }
    pub(crate) mod address_space {
        use super::VmError;
        #[derive(Clone, Copy)]
        pub(crate) enum ZapMode {
            Unmap,
            RemoveWrite,
        }
        pub(crate) struct Mapping;
        impl Mapping {
            pub(crate) fn vmo(&self) -> &alloc::sync::Arc<crate::vmo::Vmo> {
                panic!("mapping ownership belongs to guest MMU fixtures")
            }
            pub(crate) fn object_page(&self, _: usize) -> usize {
                panic!("mapping offsets belong to guest MMU fixtures")
            }
            pub(crate) fn resident_pages(
                &self,
            ) -> Result<alloc::vec::Vec<(usize, usize)>, VmError> {
                panic!("hardware residency belongs to guest MMU fixtures")
            }
            pub(in crate::vm) fn harvest_accessed(
                &self,
                _: &mut [super::vmo::AccessSample],
                _: usize,
                _: core::ops::Range<usize>,
            ) -> Result<(), VmError> {
                panic!("hardware accessed harvesting belongs to guest MMU fixtures")
            }
            pub(crate) fn zap_vmo_pages(
                &self,
                _: core::ops::Range<usize>,
                _: ZapMode,
            ) -> Result<(), VmError> {
                panic!("mapping invalidation must be covered by guest MMU fixtures")
            }
        }
    }
    #[expect(
        dead_code,
        unused_imports,
        reason = "本 fixture 只执行 COW、pager 与淘汰生命周期；固定与回收入口的 re-export 由内核其余模块消费"
    )]
    #[path = "../../../tools/source-view/kernel/vm/vmo/vmo/vmo.rs"]
    pub(crate) mod vmo;
}

use vm::vmo;
