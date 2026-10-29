#![feature(allocator_ext)]
//! 执行生产 physical Page owner/reference/weak drain registry；替换物理分配、设备 IO 与 MM fence seam。
#![allow(dead_code)]

extern crate alloc;

#[path = "swap_page/allocation.rs"]
mod allocation;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;

#[path = "../../../kernel/src/memory"]
mod memory {
    pub(crate) const PAGE_SIZE: usize = 4096;
    #[path = "shared_memory/statistics.rs"]
    pub(crate) mod shared_memory;
    pub(crate) mod address {
        pub(crate) type PhysicalPageNumber = usize;
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct SharedFileId {
        pub(crate) filesystem: usize,
        pub(crate) inode: u64,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum SharedFileError {
        OutOfMemory,
        Io,
        BeyondEof,
    }
    #[derive(Debug)]
    pub(crate) struct SharedFrame {
        ppn: usize,
        bytes: std::sync::Mutex<[u8; PAGE_SIZE]>,
    }
    impl SharedFrame {
        pub(crate) fn allocate() -> Result<Self, SharedFileError> {
            if crate::FRAME_OOM.with(|value| value.replace(false)) {
                return Err(SharedFileError::OutOfMemory);
            }
            crate::allocation::LIVE_FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Self {
                ppn: crate::NEXT_FRAME.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                bytes: std::sync::Mutex::new([0; PAGE_SIZE]),
            })
        }
        pub(crate) fn copy_frame(&self) -> Option<Self> {
            let frame = Self::allocate().ok()?;
            frame.write(0, &*self.bytes.lock().unwrap());
            Some(frame)
        }
        pub(crate) fn ppn(&self) -> usize {
            self.ppn
        }
        pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
            self.bytes.get_mut().unwrap()
        }
        pub(crate) fn read(&self, offset: usize, output: &mut [u8]) {
            output.copy_from_slice(&self.bytes.lock().unwrap()[offset..offset + output.len()]);
        }
        pub(crate) fn write(&self, offset: usize, input: &[u8]) {
            self.bytes.lock().unwrap()[offset..offset + input.len()].copy_from_slice(input);
        }
        pub(crate) fn zero_from(&self, offset: usize) {
            self.bytes.lock().unwrap()[offset..].fill(0);
        }
    }
    impl Drop for SharedFrame {
        fn drop(&mut self) {
            crate::allocation::LIVE_FRAMES.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    pub(crate) mod frame_allocator {
        pub(crate) type FrameTracker = super::SharedFrame;
    }
    pub(crate) mod shared_file {
        pub(crate) fn revoke_swappable_page(
            page: &super::swappable_page::SwappablePage,
            ppn: usize,
        ) -> Option<usize> {
            crate::REVOKE
                .with(|hook| hook.borrow_mut().as_mut().expect("configured MM seam")(page, ppn))
        }
    }
    pub(crate) mod swap {
        pub(crate) use crate::storage::{SwapError, SwapSlot, reserve};
    }
    pub(crate) mod swappable_page;
    pub(crate) mod vmstat;
    mod tests {
        use super::swappable_page::{PageMappingRef, SwappablePage};
        use crate::{FRAME_OOM, REVOKE, WRITE_HOOK, memory, storage};
        use std::sync::{Arc, atomic::Ordering};
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/swap_page/cases.rs"
        ));
    }
}

#[path = "swap_page/storage.rs"]
mod storage;

use memory::swappable_page::SwappablePage;
use std::{
    cell::{Cell, RefCell},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

type Revoke = Box<dyn FnMut(&SwappablePage, usize) -> Option<usize>>;
thread_local! {
    static REVOKE: RefCell<Option<Revoke>> = const { RefCell::new(None) };
    static FRAME_OOM: Cell<bool> = const { Cell::new(false) };
    static READ_HOOK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    static WRITE_HOOK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}
static NEXT_FRAME: AtomicUsize = AtomicUsize::new(1);

pub(crate) mod execution {
    pub(crate) mod sync {
        pub(crate) type TaskMutexGuard<'a, T> = std::sync::MutexGuard<'a, T>;
        pub(crate) struct TaskMutex<T>(std::sync::Mutex<T>);
        impl<T> TaskMutex<T> {
            pub(crate) fn new(value: T) -> Self {
                Self(std::sync::Mutex::new(value))
            }
            pub(crate) fn lock(&self) -> Result<TaskMutexGuard<'_, T>, ()> {
                self.0.lock().map_err(|_| ())
            }
            pub(crate) fn try_lock(&self) -> Option<TaskMutexGuard<'_, T>> {
                self.0.try_lock().ok()
            }
        }
    }
}
