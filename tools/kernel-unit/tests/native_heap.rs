//! Actual service allocator wiring (VMAR/VMO backing); the host VMAR seam retains mapped ranges and accounts retirement.
use std::alloc::{GlobalAlloc, Layout};
use std::ops::Range;
use std::sync::Mutex;

mod sync {
    pub(crate) use spin::Mutex;
}
struct Vmo(usize);
impl Vmo {
    fn create(size: u64, _: u32) -> Result<Self, ()> {
        if FAIL_MAP.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return Err(());
        }
        Ok(Self(size as usize))
    }
    fn op_range(&self, op: u32, _: u64, _: u64) -> Result<(), ()> {
        assert_eq!(op, native_abi::vmo::OP_DECOMMIT);
        DECOMMITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}
static DECOMMITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct Mapping {
    allocation: usize,
    layout: Layout,
    range: Range<usize>,
    live: Vec<Range<usize>>,
}
static MAPPINGS: Mutex<Vec<Mapping>> = Mutex::new(Vec::new());
// One test owns one-shot backend failures; they do not coordinate production allocation state.
static FAIL_MAP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static FAIL_TRIM: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static MAPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct Vmar;
impl Vmar {
    fn map(&self, _: u32, _: usize, vmo: &Vmo, _: u64, size: usize) -> Result<usize, ()> {
        assert_eq!(size, vmo.0);
        // Deliberately start two pages after a 64-KiB boundary to exercise the real trimming path.
        let layout = Layout::from_size_align(size + 65536, 65536).unwrap();
        // SAFETY: the returned mapping retains this allocation until all live ranges are unmapped.
        let allocation = unsafe { std::alloc::alloc_zeroed(layout) } as usize;
        if allocation == 0 {
            return Err(());
        }
        let base = allocation + 2 * native_abi::PAGE_SIZE;
        MAPPINGS.lock().unwrap().push(Mapping {
            allocation,
            layout,
            range: base..base + size,
            live: std::iter::once(base..base + size).collect(),
        });
        MAPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(base)
    }
    unsafe fn unmap(&self, base: usize, size: usize) -> Result<(), ()> {
        let end = base + size;
        let mut mappings = MAPPINGS.lock().unwrap();
        if FAIL_TRIM.try_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |remaining| remaining.checked_sub(1),
        ) == Ok(1)
        {
            return Err(());
        }
        let mapping_index = mappings
            .iter()
            .position(|mapping| mapping.range.start <= base && end <= mapping.range.end)
            .ok_or(())?;
        let mapping = &mut mappings[mapping_index];
        let live = std::mem::take(&mut mapping.live);
        for range in live {
            if end <= range.start || range.end <= base {
                mapping.live.push(range);
            } else {
                if range.start < base {
                    mapping.live.push(range.start..base);
                }
                if end < range.end {
                    mapping.live.push(end..range.end);
                }
            }
        }
        if mapping.live.is_empty() {
            let retired = mappings.swap_remove(mapping_index);
            drop(mappings);
            // SAFETY: no live virtual range remains and the allocator relinquished this block.
            unsafe { std::alloc::dealloc(retired.allocation as *mut u8, retired.layout) };
        }
        Ok(())
    }
}
mod futex {
    use std::sync::atomic::AtomicU32;
    pub(crate) fn wait(_: &AtomicU32, _: u32, _: i64, _: u32) -> Result<(), i32> {
        std::thread::yield_now();
        Ok(())
    }
    pub(crate) fn wake(_: &AtomicU32, _: u32, _: u32) {}
}
mod thread {
    use span_heap::LocalHeap;
    use std::cell::Cell;
    use std::sync::atomic::AtomicPtr;
    std::thread_local! {
        static SLOT: Cell<Option<&'static AtomicPtr<LocalHeap>>> = const { Cell::new(None) };
    }
    pub(crate) fn heap_slot() -> Option<&'static AtomicPtr<LocalHeap>> {
        Some(SLOT.with(|cell| match cell.get() {
            Some(slot) => slot,
            None => {
                let slot: &'static AtomicPtr<LocalHeap> =
                    Box::leak(Box::new(AtomicPtr::new(std::ptr::null_mut())));
                cell.set(Some(slot));
                slot
            }
        }))
    }
}
#[path = "../../../system/native-runtime/src/heap"]
mod heap {
    use super::*;
    struct Installed;
    static VMAR: Installed = Installed;
    impl Installed {
        fn get(&self) -> Option<&'static Vmar> {
            Some(vmar())
        }
    }
    fn vmar() -> &'static Vmar {
        &Vmar
    }
    mod allocator;

    #[test]
    fn rpc_buffers_reuse_aligned_nonoverlapping_blocks_without_vmar_churn() {
        use std::sync::atomic::Ordering;
        let heap = allocator::Heap;
        // 先分配一次让区域与 span 就位，之后同尺寸的反复分配/释放不得再建立映射。
        for (size, align) in [
            (2048, 16),
            (8 + 8 + 4 + device_model::PCM_PERIOD_BYTES, 16),
            (65536, 65536),
            (native_abi::CHANNEL_MAX_BYTES, 16),
        ] {
            let layout = Layout::from_size_align(size, align).unwrap();
            // SAFETY: valid nonzero layouts; every allocation is returned exactly once below.
            unsafe {
                let first = heap.alloc(layout);
                let second = heap.alloc(layout);
                assert!(!first.is_null() && !second.is_null());
                assert_ne!(first, second);
                assert_eq!(first as usize % align, 0);
                assert_eq!(second as usize % align, 0);
                first.write_bytes(0x35, size);
                second.write_bytes(0x79, size);
                let admitted = MAPS.load(Ordering::Relaxed);
                heap.dealloc(first, layout);
                for _ in 0..32 {
                    let reused = heap.alloc(layout);
                    assert_eq!(
                        MAPS.load(Ordering::Relaxed),
                        admitted,
                        "RPC buffers must not churn VMAR mappings"
                    );
                    assert_eq!(
                        *second.add(size - 1),
                        0x79,
                        "reusing one live class block must not alias another"
                    );
                    heap.dealloc(reused, layout);
                }
                heap.dealloc(second, layout);
            }
        }
    }

    #[test]
    fn a_completed_burst_returns_physical_pages_without_unmapping() {
        use std::sync::atomic::Ordering;
        let heap = allocator::Heap;
        let layout = Layout::from_size_align(native_abi::CHANNEL_MAX_BYTES, 16).unwrap();
        let mappings = MAPPINGS.lock().unwrap().len();
        let decommits = DECOMMITS.load(Ordering::Relaxed);
        // SAFETY: model concurrent RPC replies, then return every buffer before checking retention.
        unsafe {
            let replies: Vec<_> = (0..64).map(|_| heap.alloc(layout)).collect();
            assert!(replies.iter().all(|pointer| !pointer.is_null()));
            for reply in replies {
                heap.dealloc(reply, layout);
            }
        }
        assert!(
            DECOMMITS.load(Ordering::Relaxed) > decommits,
            "empty spans beyond the retained count must be decommitted"
        );
        assert!(
            MAPPINGS.lock().unwrap().len() >= mappings,
            "regions stay mapped; only physical pages are returned"
        );
    }

    #[test]
    fn a_retired_thread_heap_hands_its_live_blocks_to_other_threads() {
        let layout = Layout::from_size_align(96, 16).unwrap();
        let live = std::thread::spawn(move || {
            let heap = allocator::Heap;
            // SAFETY: valid layout; the block outlives the thread and is freed below exactly once.
            let block = unsafe { heap.alloc(layout) } as usize;
            allocator::retire_thread(super::thread::heap_slot().unwrap());
            block
        })
        .join()
        .unwrap();
        // SAFETY: the block was allocated above and not freed; freeing from another thread uses
        // the remote path into the orphaned span.
        unsafe { allocator::Heap.dealloc(live as *mut u8, layout) };
    }

    #[test]
    fn rejected_mappings_leave_the_allocator_usable_and_large_maps_retire() {
        use std::sync::atomic::Ordering;
        let heap = allocator::Heap;
        // 大对象（> 256 KiB）失败的 VMO 创建不能留下半个映射，之后可重试。
        let layout = Layout::from_size_align(1 << 20, 16).unwrap();
        FAIL_MAP.store(true, Ordering::Relaxed);
        // SAFETY: a rejected mapping must leave the allocator empty and usable for a later retry.
        unsafe {
            assert!(heap.alloc(layout).is_null());
            let retry = heap.alloc(layout);
            assert!(!retry.is_null());
            heap.dealloc(retry, layout);
        }
        let before = MAPPINGS.lock().unwrap().len();
        let layout = Layout::from_size_align(1 << 20, 65536).unwrap();
        FAIL_TRIM.store(2, Ordering::Relaxed);
        // SAFETY: no failed aligned allocation may escape or retain its unpublished mapping.
        unsafe {
            assert!(heap.alloc(layout).is_null());
            assert_eq!(MAPPINGS.lock().unwrap().len(), before);
            let retry = heap.alloc(layout);
            assert!(!retry.is_null());
            heap.dealloc(retry, layout);
        }
        let before = MAPPINGS.lock().unwrap().len();
        let layout = Layout::from_size_align(1 << 20, 65536).unwrap();
        // SAFETY: the independent mapping is only accessed while allocated, then fully retired.
        unsafe {
            let large = heap.alloc(layout);
            assert!(!large.is_null());
            assert_eq!(large as usize % layout.align(), 0);
            large.add(layout.size() - 1).write(0xa5);
            assert_eq!(MAPPINGS.lock().unwrap().len(), before + 1);
            heap.dealloc(large, layout);
        }
        assert_eq!(MAPPINGS.lock().unwrap().len(), before);
    }
}
