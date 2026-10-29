#![feature(allocator_ext)]
#![allow(dead_code)]
//! Production context storage/ContextOwner/TaskMutex; host mapped frame/page table and scheduler seams.
extern crate alloc;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
thread_local! { static FAIL_NEXT: Cell<bool> = const { Cell::new(false) }; }
struct FaultAllocator;
// SAFETY: failure injection is thread-local; every successful allocation delegates to System.
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
mod scheduler_target;
#[path = "../../../kernel/src/execution/task/model"]
mod model {
    use crate::execution::sync::TaskMutex;
    use alloc::sync::Arc;
    #[derive(Debug)]
    pub(crate) enum MemoryError {
        OutOfMemory,
        InvalidRange,
    }
    #[derive(Clone)]
    pub(crate) struct UserContext {
        cookie: usize,
    }
    #[repr(align(4096))]
    struct Frame([u8; 4096]);
    struct PhysicalPageNumber(usize);
    impl PhysicalPageNumber {
        // SAFETY: frame is a unique host allocation, pinned by MemorySet/AddressSpace owner.
        unsafe fn as_page_mut_ptr(&self) -> *mut u8 {
            self.0 as *mut u8
        }
    }
    struct VirtualAddress(usize);
    impl From<usize> for VirtualAddress {
        fn from(value: usize) -> Self {
            Self(value)
        }
    }
    impl VirtualAddress {
        fn page_offset(&self) -> usize {
            self.0 % 4096
        }
    }
    pub(crate) struct MemorySet {
        frame: Box<Frame>,
        mappings: usize,
    }
    impl MemorySet {
        fn allocate_thread_trap_context(&mut self) -> Result<usize, MemoryError> {
            // host mapping seam: production insert_framed_area transaction is tested by memory fixtures.
            self.mappings += 1;
            Ok(self.mappings * 4096)
        }
        fn trap_context_ppn(&self, _: usize) -> PhysicalPageNumber {
            PhysicalPageNumber(core::ptr::from_ref(self.frame.as_ref()).addr())
        }
    }
    pub(crate) struct AddressSpace {
        memory_set: TaskMutex<MemorySet>,
    }
    impl AddressSpace {
        fn new(cookie: usize) -> Arc<Self> {
            let mut frame = Box::new(Frame([0; 4096]));
            // SAFETY: aligned storage is large enough and contains no live UserContext yet.
            unsafe {
                frame
                    .0
                    .as_mut_ptr()
                    .cast::<UserContext>()
                    .write(UserContext { cookie });
            }
            Arc::new(Self {
                memory_set: TaskMutex::new(MemorySet { frame, mappings: 0 }),
            })
        }
    }
    mod user_context;
    use user_context::{ContextBacking, ContextBinding, ContextOwner};
    mod context_storage;

    #[test]
    fn prepared_exec_pointer_commits_while_registered_new_mm_is_locked_without_allocation() {
        let old = AddressSpace::new(11);
        let new = AddressSpace::new(22);
        let owner = old
            .bind_user_context(ContextBinding::address_space(4096))
            .unwrap();
        let prepared = new.prepare_context_rebind(&owner, 4096).unwrap().unwrap();
        let guard = new.memory_set.lock().unwrap(); // legitimate mapping/reclaimer callback contention
        crate::FAIL_NEXT.set(true);
        prepared.apply(&owner);
        assert!(crate::FAIL_NEXT.replace(false));
        assert_eq!(owner.with(|context| context.cookie), 22);
        drop(guard);
    }

    #[test]
    fn prepared_pointer_pins_the_address_space_until_commit() {
        let old = AddressSpace::new(11);
        let new = AddressSpace::new(22);
        let observer = Arc::downgrade(&new);
        let owner = old
            .bind_user_context(ContextBinding::address_space(4096))
            .unwrap();
        let prepared = new.prepare_context_rebind(&owner, 4096).unwrap().unwrap();
        drop(new);
        let published = observer.upgrade().expect("prepared storage pins new root");
        prepared.apply(&owner);
        assert_eq!(owner.with(|context| context.cookie), 22);
        owner.retire(); // retire before last root/frame Drop
        drop(published);
        assert!(observer.upgrade().is_none());
    }

    #[test]
    fn allocation_waiter_oom_happens_before_mapping_publication() {
        let memory = AddressSpace::new(33);
        let target = crate::scheduler_target::Target::new();
        let guard = memory.memory_set.lock().unwrap();
        let join = std::thread::spawn({
            let memory = memory.clone();
            move || {
                let _binding = target.bind();
                crate::FAIL_NEXT.set(true);
                let failed = memory.allocate_thread_context().is_err();
                assert!(!crate::FAIL_NEXT.get());
                failed
            }
        });
        assert!(join.join().unwrap());
        assert_eq!(guard.mappings, 0);
    }

    #[test]
    fn contended_preparation_maps_and_binds_in_one_guard() {
        let memory = AddressSpace::new(33);
        let target = crate::scheduler_target::Target::new();
        let guard = memory.memory_set.lock().unwrap();
        let join = std::thread::spawn({
            let memory = memory.clone();
            let target = target.clone();
            move || {
                let _binding = target.bind();
                let context = memory.allocate_thread_context().unwrap();
                assert_eq!(context.binding().address(), 4096);
                assert_eq!(context.with(|context| context.cookie), 33);
                context.retire();
            }
        });
        target.wait_until_sleeping();
        assert_eq!(guard.mappings, 0);
        drop(guard);
        join.join().unwrap();
        assert_eq!(memory.memory_set.lock().unwrap().mappings, 1);
    }
}
mod memory {
    pub(crate) const PAGE_SIZE: usize = 4096;
}

#[path = "."]
pub(crate) mod execution {
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
        pub(crate) use task_mutex::TaskMutex;
    }
}
