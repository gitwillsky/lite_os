#![feature(allocator_ext)]
#![allow(dead_code)]
//! Production Thread mm handle/release token; user-copy and constructors are explicit host seams.
extern crate alloc;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
thread_local! { static FAIL_NEXT: Cell<bool> = const { Cell::new(false) }; static WAKE_CALLS: Cell<usize> = const { Cell::new(0) }; }
struct FaultAllocator;
// SAFETY: thread-local failure injection; successful allocation/deallocation delegates to System.
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
mod memory {
    pub(crate) use crate::execution::task::model::MemorySet;
    #[derive(Clone, Copy)]
    pub(crate) struct UserFaultLimits;
    #[derive(Debug)]
    pub(crate) enum MemoryError {
        OutOfMemory,
    }
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
        #[allow(unused_imports, reason = "本 fixture 不执行 signal-acquisition 接口")]
        mod task_mutex;
        pub(crate) use task_mutex::{TaskMutex, TaskMutexWaitPreparation};
    }
    #[path = "../../../kernel/src/execution/task"]
    pub(crate) mod task {
        pub(crate) mod process_table {
            pub(crate) fn futex_wake_with_key(
                count: usize,
                bitset: u32,
                preparation: &mut crate::execution::sync::TaskMutexWaitPreparation,
                with_key: impl FnOnce(
                    &mut crate::execution::sync::TaskMutexWaitPreparation,
                    &mut dyn FnMut(usize),
                ) -> Result<(), ()>,
            ) -> Result<usize, ()> {
                assert_eq!((count, bitset), (1, u32::MAX));
                with_key(preparation, &mut |key| assert_eq!(key, 17))?;
                crate::WAKE_CALLS.with(|count| count.set(count.get() + 1));
                Ok(1)
            }
        }
        #[path = "model"]
        pub(crate) mod model {
            use crate::memory::UserFaultLimits;
            use alloc::sync::Arc;
            use spin::Mutex;
            use std::sync::{
                Mutex as HostMutex,
                atomic::{AtomicBool, Ordering},
            };
            #[derive(Default)]
            pub(crate) struct Observations {
                pub(crate) clears: HostMutex<Vec<(usize, [u8; 4])>>,
                pub(crate) keys: HostMutex<Vec<usize>>,
            }
            pub(crate) struct MemorySet {
                pub(crate) observations: Arc<Observations>,
                writable: bool,
                readable: bool,
            }
            impl MemorySet {
                pub(crate) fn copy_to_user(
                    &mut self,
                    address: usize,
                    bytes: &[u8],
                    _: UserFaultLimits,
                ) -> Result<(), ()> {
                    if !self.writable {
                        return Err(());
                    }
                    self.observations
                        .clears
                        .lock()
                        .unwrap()
                        .push((address, bytes.try_into().unwrap()));
                    Ok(())
                }
                pub(crate) fn futex_key(
                    &mut self,
                    address: usize,
                    _: usize,
                    private: bool,
                    _: UserFaultLimits,
                ) -> Result<usize, ()> {
                    assert!(!private);
                    self.observations.keys.lock().unwrap().push(address);
                    if self.readable { Ok(17) } else { Err(()) }
                }
            }
            pub(crate) struct AddressSpace {
                memory_set: crate::execution::sync::TaskMutex<MemorySet>,
                dumpability: dumpability::Dumpability,
            }
            #[path = "address_space/dumpability.rs"]
            mod dumpability;
            impl AddressSpace {
                fn dumpable(&self) -> bool {
                    self.dumpability.user_dumpable()
                }
                fn set_dumpable(&self, user: bool) {
                    self.dumpability.set_user_dumpable(user);
                }
            }
            struct Process {
                leader_mm: Mutex<thread_mm::MmHandle>,
            }
            struct Thread {
                mm: thread_mm::MmHandle,
                memory_cleanup_wait:
                    Mutex<Option<crate::execution::sync::TaskMutexWaitPreparation>>,
                clear_child_tid: Mutex<Option<usize>>,
            }
            pub(crate) struct TaskControlBlock {
                process: Arc<Process>,
                thread: Thread,
                robust_cleaned: AtomicBool,
                context_retired: AtomicBool,
                fs_retired: AtomicBool,
                files_retired: AtomicBool,
            }
            impl TaskControlBlock {
                fn process(&self) -> &Process {
                    &self.process
                }
                // 此 retirement fixture 只构造 user execution；kernel absence 由 proc guest 覆盖。
                fn is_kernel_thread(&self) -> bool {
                    false
                }
                fn thread(&self) -> &Thread {
                    &self.thread
                }
                fn user_fault_limits(&self) -> UserFaultLimits {
                    UserFaultLimits
                }
                fn cleanup_robust_list(&self) {
                    assert!(!self.context_retired.load(Ordering::Relaxed));
                    self.robust_cleaned.store(true, Ordering::Relaxed);
                }
                fn remove_thread_trap_context(&self) {
                    assert!(self.robust_cleaned.load(Ordering::Relaxed));
                    self.context_retired.store(true, Ordering::Relaxed);
                }
                fn retire_fs_context(&self) {
                    assert!(self.context_retired.load(Ordering::Relaxed));
                    self.fs_retired.store(true, Ordering::Relaxed);
                }
                fn retire_namespaces(&self) {
                    // This MM-retirement fixture owns no namespace membership; actual exit is covered separately.
                    assert!(self.files_retired.load(Ordering::Relaxed));
                }
                fn retire_semaphore_undo(&self) {
                    // 此 MM 用例没有 SEM_UNDO membership；实际 IPC owner 在 ipc_semaphores 验证。
                }
                fn retire_shared_memory_creator(&self) {
                    // 此 MM 用例没有 shm clist；实际 creator membership 在 ipc_shared_memory 验证。
                }
                fn record_peak_rss(&self, _: &AddressSpace) {
                    // 同上：exec 释放旧映像时的峰值并入不属于此用例。
                }
                fn record_exit_peak_rss(&self) {
                    // 此 MM-retirement 用例不关心 rusage 峰值；采样语义由 musl smoke 的 resource-usage 用例覆盖。
                }
                fn retire_seccomp(&self) {
                    // Explicit seam: real listener task membership is covered by seccomp_notifications.
                }
                fn retire_file_table(&self) {
                    assert!(self.fs_retired.load(Ordering::Relaxed));
                    self.files_retired.store(true, Ordering::Relaxed);
                }
                fn take_clear_child_tid(&self) -> Option<usize> {
                    self.thread.clear_child_tid.lock().take()
                }
                fn new(mm: Arc<AddressSpace>, tid: Option<usize>) -> Arc<Self> {
                    let mm = thread_mm::try_mm_handle(mm).unwrap();
                    Arc::new(Self {
                        process: Arc::new(Process {
                            leader_mm: Mutex::new(mm.clone()),
                        }),
                        thread: Thread {
                            mm,
                            memory_cleanup_wait: Mutex::new(Some(
                                crate::execution::sync::TaskMutexWaitPreparation::prepare()
                                    .unwrap(),
                            )),
                            clear_child_tid: Mutex::new(tid),
                        },
                        robust_cleaned: false.into(),
                        context_retired: false.into(),
                        fs_retired: false.into(),
                        files_retired: false.into(),
                    })
                }
                // constructor seam; production try_mm_handle is actual unique outer handle.
                fn sibling(&self) -> Arc<Self> {
                    let mm = thread_mm::try_mm_handle(self.address_space()).unwrap();
                    Arc::new(Self {
                        process: self.process.clone(),
                        thread: Thread {
                            mm,
                            memory_cleanup_wait: Mutex::new(Some(
                                crate::execution::sync::TaskMutexWaitPreparation::prepare()
                                    .unwrap(),
                            )),
                            clear_child_tid: Mutex::new(None),
                        },
                        robust_cleaned: false.into(),
                        context_retired: false.into(),
                        fs_retired: false.into(),
                        files_retired: false.into(),
                    })
                }
            }
            mod process_exit;
            mod thread_mm;
            fn root(writable: bool) -> (Arc<AddressSpace>, Arc<Observations>) {
                let observations = Arc::new(Observations {
                    clears: HostMutex::new(Vec::with_capacity(16)),
                    keys: HostMutex::new(Vec::with_capacity(16)),
                });
                // 1. macOS 的 std Mutex 基于 pthread，首次 lock 才堆分配 64 字节（Linux futex 版本不分配）。
                // 2. 测试在 FAIL_NEXT 武装窗口内断言被测内核路径不分配；若观察用 Mutex 在窗口内首次
                //    加锁，host 脚手架自己的分配会消耗注入的失败并 abort，掩盖真正的断言。
                drop(observations.clears.lock().unwrap());
                drop(observations.keys.lock().unwrap());
                (
                    Arc::new(AddressSpace {
                        dumpability: dumpability::Dumpability::new(true),
                        memory_set: crate::execution::sync::TaskMutex::new(MemorySet {
                            observations: observations.clone(),
                            writable,
                            readable: true,
                        }),
                    }),
                    observations,
                )
            }
            #[test]
            fn mm_handle_oom_releases_only_unpublished_user_and_preserves_parent() {
                let (mm, _) = root(true);
                let weak = Arc::downgrade(&mm);
                let task = TaskControlBlock::new(mm, None);
                crate::FAIL_NEXT.set(true);
                assert!(thread_mm::try_mm_handle(task.address_space()).is_err());
                assert!(!crate::FAIL_NEXT.get());
                assert!(task.has_mm());
                assert_eq!(weak.strong_count(), 1);
                let (orphan, _) = root(true);
                let orphan_weak = Arc::downgrade(&orphan);
                crate::FAIL_NEXT.set(true);
                assert!(thread_mm::try_mm_handle(orphan).is_err());
                assert!(!crate::FAIL_NEXT.get());
                assert!(orphan_weak.upgrade().is_none());
            }

            #[test]
            #[ignore = "diagnostic CPU measurement; run alone in release"]
            fn mm_handle_lookup_cpu_cost() {
                use std::{hint::black_box, time::Instant};
                let (mm, _) = root(true);
                let task = TaskControlBlock::new(mm, None);
                for _ in 0..10_000 {
                    black_box(task.address_space());
                }
                let mut samples = [0f64; 5];
                for sample in &mut samples {
                    let start = Instant::now();
                    for _ in 0..100_000 {
                        black_box(task.address_space());
                    }
                    *sample = start.elapsed().as_nanos() as f64 / 100_000.;
                }
                samples.sort_by(f64::total_cmp);
                println!("Thread mmget/drop median {:.2} ns", samples[2]);
            }

            #[test]
            fn last_mm_user_consumes_registration_without_writing_or_waking() {
                let (mm, observations) = root(true);
                let weak = Arc::downgrade(&mm);
                let task = TaskControlBlock::new(mm, Some(0x4000));
                let observer = task.clone();
                let completion = task.prepare_user_exit_memory();
                assert!(observer.files_retired.load(Ordering::Relaxed));
                assert!(!observer.has_mm());
                assert!(observer.process.process_address_space().is_none());
                assert!(observer.thread.clear_child_tid.lock().is_none());
                completion.complete();
                assert!(observations.clears.lock().unwrap().is_empty());
                assert!(observations.keys.lock().unwrap().is_empty());
                assert!(weak.upgrade().is_none()); // external TCB/Process do not retain mm
            }
            #[test]
            fn shared_mm_exit_moves_one_user_and_publishes_clear_before_last_user_drop() {
                let (mm, observations) = root(true);
                let weak = Arc::downgrade(&mm);
                let task = TaskControlBlock::new(mm, Some(0x4000));
                let sibling = task.sibling();
                let completion = task.prepare_user_exit_memory();
                assert!(!task.has_mm());
                assert!(sibling.has_mm());
                assert!(sibling.process.process_address_space().is_none()); // root proc observes original leader
                assert_eq!(weak.strong_count(), 2); // exact sibling+token, no duplicated snapshot
                completion.complete();
                assert_eq!(*observations.clears.lock().unwrap(), [(0x4000, [0; 4])]);
                assert_eq!(*observations.keys.lock().unwrap(), [0x4000]);
                assert_eq!(weak.strong_count(), 1);
                sibling.prepare_user_exit_memory().complete();
                assert!(weak.upgrade().is_none());
            }
            #[test]
            fn proc_or_registry_mmget_view_is_a_real_mapping_user() {
                let (mm, observations) = root(true);
                let task = TaskControlBlock::new(mm, Some(0x4000));
                let view = task.process.process_address_space().unwrap();
                task.prepare_user_exit_memory().complete();
                assert_eq!(*observations.keys.lock().unwrap(), [0x4000]);
                assert_eq!(Arc::strong_count(&view), 1);
            }
            #[test]
            fn exec_releases_registration_against_old_shared_root_and_never_new_same_va() {
                let (mm, observations) = root(true);
                let task = TaskControlBlock::new(mm, Some(0x4000));
                let keeper = task.sibling();
                let (new, new_observations) = root(true);
                let old = task.replace_address_space(new);
                assert_eq!(Arc::strong_count(&old), 2);
                process_exit::release_exec_mm(
                    &task,
                    old,
                    task.take_clear_child_tid(),
                    UserFaultLimits,
                );
                assert_eq!(*observations.keys.lock().unwrap(), [0x4000]);
                assert!(new_observations.clears.lock().unwrap().is_empty());
                assert!(task.thread.clear_child_tid.lock().is_none());
                assert!(keeper.has_mm());
            }
            #[test]
            fn read_only_copy_fault_still_resolves_wake_but_absent_registration_does_not() {
                for tid in [None, Some(0x4000)] {
                    let (mm, observations) = root(false);
                    let task = TaskControlBlock::new(mm, tid);
                    let keeper = task.sibling();
                    crate::WAKE_CALLS.set(0);
                    task.prepare_user_exit_memory().complete();
                    assert!(!task.has_mm());
                    assert!(keeper.has_mm());
                    assert!(observations.clears.lock().unwrap().is_empty());
                    assert_eq!(
                        observations.keys.lock().unwrap().len(),
                        usize::from(tid.is_some())
                    );
                    assert_eq!(crate::WAKE_CALLS.get(), usize::from(tid.is_some()));
                }
            }
            #[test]
            fn unreadable_word_still_attempts_key_but_does_not_wake() {
                let (mm, observations) = root(false);
                mm.memory_set.lock().unwrap().readable = false;
                let task = TaskControlBlock::new(mm, Some(0x4000));
                let _keeper = task.sibling();
                crate::WAKE_CALLS.set(0);
                task.prepare_user_exit_memory().complete();
                assert_eq!(*observations.keys.lock().unwrap(), [0x4000]);
                assert_eq!(crate::WAKE_CALLS.get(), 0);
                assert!(!task.has_mm());
            }
            #[test]
            fn contended_old_mm_release_uses_prepared_waiter_without_metadata_allocation() {
                let (mm, observations) = root(true);
                let task = TaskControlBlock::new(mm, Some(0x4000));
                let keeper = task.sibling();
                let completion = task.prepare_user_exit_memory();
                let held_mm = keeper.address_space();
                let guard = held_mm.memory_set.lock().unwrap();
                let target = crate::scheduler_target::Target::new();
                let join = std::thread::spawn({
                    let target = target.clone();
                    move || {
                        let _binding = target.bind();
                        crate::FAIL_NEXT.set(true);
                        completion.complete();
                        assert!(crate::FAIL_NEXT.replace(false));
                    }
                });
                target.wait_until_sleeping();
                assert!(observations.clears.lock().unwrap().is_empty());
                drop(guard);
                join.join().unwrap();
                assert_eq!(*observations.clears.lock().unwrap(), [(0x4000, [0; 4])]);
                assert_eq!(*observations.keys.lock().unwrap(), [0x4000]);
            }
            #[test]
            fn live_cleanup_waiter_reuses_after_blocking_and_exec_registration_release() {
                let (mm, observations) = root(true);
                let task = TaskControlBlock::new(mm, Some(0x4000));
                let keeper = task.sibling();
                let held_mm = task.address_space();
                let guard = held_mm.memory_set.lock().unwrap();
                let target = crate::scheduler_target::Target::new();
                let join = std::thread::spawn({
                    let target = target.clone();
                    let task = task.clone();
                    move || {
                        let _binding = target.bind();
                        crate::FAIL_NEXT.set(true);
                        task.with_cleanup_memory(&task.address_space(), |memory| {
                            memory
                                .copy_to_user(0x4000, &[1; 4], UserFaultLimits)
                                .unwrap()
                        });
                        assert!(crate::FAIL_NEXT.replace(false));
                        let (new, _) = root(true);
                        let old = task.replace_address_space(new);
                        process_exit::release_exec_mm(
                            &task,
                            old,
                            task.take_clear_child_tid(),
                            UserFaultLimits,
                        );
                        assert!(task.thread.memory_cleanup_wait.lock().is_some());
                        task.with_cleanup_memory(&task.address_space(), |_| ());
                    }
                });
                target.wait_until_sleeping();
                drop(guard);
                join.join().unwrap();
                assert_eq!(
                    *observations.clears.lock().unwrap(),
                    [(0x4000, [1; 4]), (0x4000, [0; 4])]
                );
                assert!(keeper.has_mm());
            }
        }
    }
}
