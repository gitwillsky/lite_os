//! 生产 span 堆的 blocking benchmark：小对象分配/释放、跨线程释放与 Vec 式增长到 64 KiB。
//!
//! 使用宿主 `Backing`（不含 VMO/decommit 系统调用成本），只约束分配器自身的 CPU 路径；
//! 上限取宽但真实的绝对值，用来阻止热路径重新引入锁、系统调用或复杂度退化。
use span_heap::{Heap, host::HostBacking};
use std::{alloc::Layout, hint::black_box, sync::Arc, time::Instant};

/// 小对象一次 alloc+free 的上限（纳秒）。
const SMALL_PAIR_LIMIT_NS: f64 = 150.0;
/// 跨线程释放（含生产者分配）每块的上限（纳秒）。
const REMOTE_FREE_LIMIT_NS: f64 = 1500.0;
/// 从 16 字节 realloc 倍增到 64 KiB 的整个序列上限（纳秒）。
const GROWTH_LIMIT_NS: f64 = 30_000.0;

fn median(mut samples: [f64; 7]) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[3]
}

fn layout(size: usize) -> Layout {
    Layout::from_size_align(size, 8).unwrap()
}

fn small_pair(heap: &Heap<HostBacking>, size: usize) -> f64 {
    let layout = layout(size);
    let operation = || {
        // SAFETY: 布局有效，块立即释放。
        unsafe {
            let pointer = heap.alloc(black_box(layout));
            black_box(pointer);
            heap.dealloc(pointer, layout);
        }
    };
    for _ in 0..10_000 {
        operation();
    }
    median(std::array::from_fn(|_| {
        let start = Instant::now();
        for _ in 0..200_000 {
            operation();
        }
        start.elapsed().as_nanos() as f64 / 200_000.0
    }))
}

fn remote_free(heap: &Arc<Heap<HostBacking>>) -> f64 {
    const BLOCKS: usize = 200_000;
    median(std::array::from_fn(|_| {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<Vec<usize>>(4);
        let consumer = {
            let heap = heap.clone();
            std::thread::spawn(move || {
                while let Ok(batch) = receiver.recv() {
                    for address in batch {
                        // SAFETY: 生产者分配的 64 字节块，恰好释放一次。
                        unsafe { heap.dealloc(address as *mut u8, layout(64)) };
                    }
                }
            })
        };
        let start = Instant::now();
        for _ in 0..BLOCKS / 1000 {
            // SAFETY: 布局有效。
            let batch = (0..1000)
                .map(|_| unsafe { heap.alloc(layout(64)) } as usize)
                .collect();
            sender.send(batch).unwrap();
        }
        drop(sender);
        consumer.join().unwrap();
        start.elapsed().as_nanos() as f64 / BLOCKS as f64
    }))
}

fn growth(heap: &Heap<HostBacking>) -> f64 {
    let run = || {
        // SAFETY: 每一步按当前布局 realloc，最后按最终布局释放。
        unsafe {
            let mut size = 16;
            let mut pointer = heap.alloc(layout(size));
            while size < 64 * 1024 {
                pointer = heap.realloc(pointer, layout(size), size * 2);
                size *= 2;
            }
            black_box(pointer);
            heap.dealloc(pointer, layout(size));
        }
    };
    for _ in 0..1000 {
        run();
    }
    median(std::array::from_fn(|_| {
        let start = Instant::now();
        for _ in 0..20_000 {
            run();
        }
        start.elapsed().as_nanos() as f64 / 20_000.0
    }))
}

fn main() {
    let heap = Arc::new(Heap::new(HostBacking::new()));
    let mut failed = false;
    let mut check = |name: &str, value: f64, limit: f64| {
        println!("span heap: {name} median_ns={value:.1} limit_ns={limit:.1}");
        failed |= value > limit;
    };
    for size in [16, 128, 1024] {
        check(
            &format!("alloc+free {size}B"),
            small_pair(&heap, size),
            SMALL_PAIR_LIMIT_NS,
        );
    }
    check(
        "cross-thread free 64B",
        remote_free(&heap),
        REMOTE_FREE_LIMIT_NS,
    );
    check("grow 16B..64KiB", growth(&heap), GROWTH_LIMIT_NS);
    if failed {
        std::process::exit(1);
    }
}
