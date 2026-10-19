#![feature(allocator_ext)]
//! 执行生产 generation allocator；仅把硬件 local fence 替换为 host adapter；使用同一生产 spin mutex。
extern crate alloc;

use std::{collections::HashMap, sync::Arc};

thread_local! { static FAIL_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) }; }
struct AllocationFault;
// SAFETY: only selected thread-local allocations return null; all other operations delegate System.
unsafe impl std::alloc::GlobalAlloc for AllocationFault {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let fail = FAIL_AFTER
            .try_with(|remaining| match remaining.get() {
                Some(0) => {
                    remaining.set(None);
                    true
                }
                Some(value) => {
                    remaining.set(Some(value - 1));
                    false
                }
                None => false,
            })
            .unwrap_or(false);
        if fail {
            core::ptr::null_mut()
        } else {
            unsafe { std::alloc::System.alloc(layout) }
        }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(pointer, layout) };
    }
}
#[global_allocator]
static ALLOCATOR: AllocationFault = AllocationFault;

#[test]
fn preparation_oom_does_not_publish_partial_allocator_or_claim_a_hardware_id() {
    for allocation in 0..3 {
        FAIL_AFTER.set(Some(allocation));
        let result = Allocator::try_new(256, 2);
        FAIL_AFTER.set(None);
        assert!(result.is_err());
    }
    let allocator = Allocator::try_new(256, 2).unwrap();
    FAIL_AFTER.set(Some(0));
    let result = Context::try_new();
    FAIL_AFTER.set(None);
    assert!(result.is_err());
    let context = Context::try_new().unwrap();
    assert_eq!(allocator.activate(&context, 0, |_| {}), 1);
}

#[path = "../../../kernel/src/arch/address_space_id.rs"]
mod address_space_id;
use address_space_id::{Allocator, Context, Flush};

#[derive(Default)]
struct LocalTranslations {
    entries: HashMap<usize, usize>,
    full_fences: usize,
    scoped_fences: usize,
}
impl LocalTranslations {
    fn activate(&mut self, allocator: &Allocator, context: &Arc<Context>, cpu: usize) -> usize {
        let identifier = allocator.activate(context, cpu, |flush| match flush {
            Flush::All => {
                self.entries.clear();
                self.full_fences += 1;
            }
            Flush::Identifier(identifier) => {
                self.entries.remove(&identifier);
                self.scoped_fences += 1;
            }
        });
        let root = Arc::as_ptr(context) as usize;
        if let Some(previous_root) = self.entries.insert(identifier, root) {
            assert_eq!(
                previous_root, root,
                "hardware ID selected a stale root translation"
            );
        }
        assert_ne!(identifier, 0);
        identifier
    }
}

#[test]
fn creating_more_roots_than_hardware_ids_is_lazy_and_fallible_only_on_storage() {
    let allocator = Allocator::try_new(8, 1).unwrap();
    let roots: Vec<_> = (0..1024).map(|_| Context::try_new().unwrap()).collect();
    let mut local = LocalTranslations::default();
    assert_eq!(local.activate(&allocator, &roots[1000], 0), 1);
    for root in &roots {
        local.activate(&allocator, root, 0);
    }
    for root in roots.iter().rev() {
        local.activate(&allocator, root, 0);
    }
    assert!(local.full_fences > 100);
}

#[test]
fn fast_reactivation_needs_no_additional_fence() {
    let allocator = Allocator::try_new(256, 2).unwrap();
    let root = Context::try_new().unwrap();
    let mut local = LocalTranslations::default();
    let identifier = local.activate(&allocator, &root, 0);
    for _ in 0..1000 {
        assert_eq!(local.activate(&allocator, &root, 0), identifier);
    }
    assert_eq!(local.full_fences, 1);
    assert_eq!(local.scoped_fences, 0);
    let other = Context::try_new().unwrap();
    local.activate(&allocator, &other, 0);
    assert_eq!(local.scoped_fences, 1);
    let mut remote = LocalTranslations::default();
    remote.activate(&allocator, &root, 1);
    remote.activate(&allocator, &other, 1);
    assert_eq!(remote.full_fences, 1);
    assert_eq!(remote.scoped_fences, 1);
}

#[test]
fn repeated_rollovers_preserve_idle_cpus_and_all_copies_of_one_active_root() {
    let allocator = Allocator::try_new(8, 2).unwrap();
    let shared = Context::try_new().unwrap();
    let mut local = LocalTranslations::default();
    let mut idle = LocalTranslations::default();
    let identifier = local.activate(&allocator, &shared, 0);
    assert_eq!(idle.activate(&allocator, &shared, 1), identifier);
    for _ in 0..20 {
        let roots: Vec<_> = (0..30).map(|_| Context::try_new().unwrap()).collect();
        for root in &roots {
            local.activate(&allocator, root, 0);
        }
        // CPU 1 没有切换，旧 generation 的 reservation 必须跨每次 rollover 保留。
        assert_eq!(local.activate(&allocator, &shared, 0), identifier);
    }
    assert_eq!(idle.activate(&allocator, &shared, 1), identifier);
    assert_eq!(idle.full_fences, 2);
}

#[test]
fn a_third_cpu_rollover_updates_both_reserved_copies_of_the_same_root() {
    let allocator = Allocator::try_new(8, 3).unwrap();
    let shared = Context::try_new().unwrap();
    let mut first = LocalTranslations::default();
    let mut second = LocalTranslations::default();
    let mut churn = LocalTranslations::default();
    let identifier = first.activate(&allocator, &shared, 0);
    assert_eq!(second.activate(&allocator, &shared, 1), identifier);
    for _ in 0..20 {
        let roots: Vec<_> = (0..20).map(|_| Context::try_new().unwrap()).collect();
        for root in &roots {
            churn.activate(&allocator, root, 2);
        }
        // CPU0/1 的 shared reservation 都需更新；下一轮不切换它们继续 capture fallback。
        assert_eq!(churn.activate(&allocator, &shared, 2), identifier);
    }
    assert_eq!(first.activate(&allocator, &shared, 0), identifier);
    assert_eq!(second.activate(&allocator, &shared, 1), identifier);
    assert_eq!(first.full_fences, 2);
    assert_eq!(second.full_fences, 2);
}

#[test]
fn late_cpu_seen_publication_after_rollover_cannot_skip_its_pending_full_fence() {
    let allocator = Allocator::try_new(8, 2).unwrap();
    let shared = Context::try_new().unwrap();
    let other = Context::try_new().unwrap();
    let mut remote = LocalTranslations::default();
    let identifier = remote.activate(&allocator, &shared, 1);
    let (captured_tx, captured_rx) = std::sync::mpsc::sync_channel(0);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(0);
    std::thread::scope(|scope| {
        let allocator_ref = &allocator;
        let shared_ref = &shared;
        let worker = scope.spawn(move || {
            let allocator = allocator_ref;
            let shared = shared_ref;
            let mut local = LocalTranslations::default();
            local.activate(allocator, &other, 0);
            let selected = allocator.activate(shared, 0, |flush| {
                assert_eq!(flush, Flush::Identifier(identifier));
                // 真正快路已经 CAS active=shared；暂停 fence/seen 以允许 CPU1 capture 它。
                captured_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
                local.entries.remove(&identifier);
            });
            assert_eq!(selected, identifier);
            local
                .entries
                .insert(identifier, Arc::as_ptr(shared) as usize);
            assert_eq!(local.activate(allocator, shared, 0), identifier);
            assert_eq!(local.full_fences, 2);
        });
        captured_rx.recv().unwrap();
        let roots: Vec<_> = (0..40).map(|_| Context::try_new().unwrap()).collect();
        for root in &roots {
            remote.activate(&allocator, root, 1);
        }
        assert_eq!(remote.activate(&allocator, &shared, 1), identifier);
        // 新 identity/seen 清零已发布，恢复旧 activation 的迟到 fetch_or。
        resume_tx.send(()).unwrap();
        worker.join().unwrap();
    });
}

#[test]
fn concurrent_activation_and_rollover_never_select_another_live_roots_cached_page() {
    let allocator = Allocator::try_new(64, 4).unwrap();
    let roots: Vec<_> = (0..257).map(|_| Context::try_new().unwrap()).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|cpu| {
                let allocator = &allocator;
                let roots = &roots;
                scope.spawn(move || {
                    let mut local = LocalTranslations::default();
                    for iteration in 0..40_000 {
                        let index = (iteration * 31 + cpu * 47) % roots.len();
                        local.activate(allocator, &roots[index], cpu);
                        if iteration % 59 == 0 {
                            std::thread::yield_now();
                        }
                    }
                    assert!(local.full_fences > 1);
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    });
}

#[test]
fn retirement_invalidates_stale_capabilities_without_reusing_an_unfenced_id() {
    let allocator = Allocator::try_new(8, 1).unwrap();
    let context = Context::try_new().unwrap();
    let id = allocator.activate(&context, 0, |_| {});
    // 生产调用者已完成 all-CPU fence；该方法只退休 software capability，不开放 bitmap 位。
    context.retire_after_global_fence();
    let next = Context::try_new().unwrap();
    assert_ne!(allocator.activate(&next, 0, |_| {}), id);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| allocator.activate(
            &context,
            0,
            |_| {}
        )))
        .is_err()
    );
    assert!(std::panic::catch_unwind(|| context.retire_after_global_fence()).is_err());
}

#[test]
fn full_hardware_bitmap_and_native_word_boundary_rollovers_preserve_zero_reservation() {
    for capacity in [8, 64, 128, 256, 65_536] {
        let allocator = Allocator::try_new(capacity, 1).unwrap();
        let roots: Vec<_> = (0..capacity + 1)
            .map(|_| Context::try_new().unwrap())
            .collect();
        let mut local = LocalTranslations::default();
        for root in &roots {
            assert!(local.activate(&allocator, root, 0) < capacity);
        }
        assert_eq!(local.full_fences, 2);
    }
}

#[test]
#[ignore = "diagnostic median CPU cost; guest TLB correctness is independently verified"]
fn production_asid_activation_cpu_cost() {
    use std::{hint::black_box, time::Instant};
    let allocator = Allocator::try_new(256, 2).unwrap();
    let context = Context::try_new().unwrap();
    allocator.activate(&context, 0, |_| {});
    for _ in 0..100_000 {
        black_box(allocator.activate(black_box(&context), 0, |_| {}));
    }
    let mut samples = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        for _ in 0..1_000_000 {
            let token = black_box(context.clone());
            black_box(allocator.activate(black_box(&token), 0, |_| {}));
            drop(token);
        }
        samples.push(started.elapsed().as_nanos() as f64 / 1_000_000.0);
    }
    samples.sort_by(f64::total_cmp);
    eprintln!(
        "production ASID fast activation plus token clone/drop: {:.2} ns/op",
        samples[2]
    );
}
