//! Actual service allocator; the host VMAR seam retains mapped ranges and accounts retirement.
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
}
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
    fn channel_buffers_reuse_aligned_nonoverlapping_blocks_and_large_maps_retire() {
        use std::sync::atomic::Ordering;
        let heap = allocator::Heap;
        for (size, align) in [(2048, 16), (65536, 65536)] {
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
        let retained = MAPPINGS.lock().unwrap().len();
        let layout = Layout::from_size_align(native_abi::CHANNEL_MAX_BYTES, 16).unwrap();
        // SAFETY: model concurrent RPC replies, then return every buffer before checking retention.
        unsafe {
            let replies: Vec<_> = (0..24).map(|_| heap.alloc(layout)).collect();
            assert!(replies.iter().all(|pointer| !pointer.is_null()));
            for reply in replies {
                heap.dealloc(reply, layout);
            }
        }
        assert_eq!(
            MAPPINGS.lock().unwrap().len(),
            retained,
            "a completed RPC burst must not permanently retain its peak reply mappings"
        );
        let retained = MAPPINGS.lock().unwrap().len();
        let layout = Layout::from_size_align(4096, 16).unwrap();
        // SAFETY: medium allocations own independent mappings and must retire on return.
        unsafe {
            let buffer = heap.alloc(layout);
            assert!(!buffer.is_null());
            heap.dealloc(buffer, layout);
        }
        assert_eq!(MAPPINGS.lock().unwrap().len(), retained);
        let layout = Layout::from_size_align(8192, 8192).unwrap();
        FAIL_MAP.store(true, Ordering::Relaxed);
        // SAFETY: a rejected chunk must leave the class empty and usable for a later retry.
        unsafe {
            assert!(heap.alloc(layout).is_null());
            let retry = heap.alloc(layout);
            assert!(!retry.is_null());
            heap.dealloc(retry, layout);
        }
        let before = MAPPINGS.lock().unwrap().len();
        let layout = Layout::from_size_align(32768, 32768).unwrap();
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
        let layout = Layout::from_size_align(131072, 65536).unwrap();
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
        // Cached chunks intentionally live with CLASSES for the service/process lifetime.
    }
}
