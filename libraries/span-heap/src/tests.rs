//! 宿主并发测试：用标准库内存实现 [`Backing`]，覆盖级别、对齐、跨线程释放、线程退出认领与归还。
use super::*;
use crate::host::{HostBacking, thread_slot as slot};
use std::sync::Arc;

fn heap() -> Arc<Heap<HostBacking>> {
    // 每个测试用独立的堆；Heap 很大（表与数组），放在堆上避免栈溢出。
    Arc::new(Heap::new(HostBacking::new()))
}

fn layout(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align).unwrap()
}

#[test]
fn blocks_are_aligned_disjoint_and_keep_their_contents() {
    let heap = heap();
    let mut live: Vec<(*mut u8, Layout, u8)> = Vec::new();
    for (index, size) in [1usize, 15, 16, 17, 100, 129, 1000, 5000, 70_000, 262_144]
        .into_iter()
        .cycle()
        .take(2000)
        .enumerate()
    {
        let align = [1usize, 8, 16, 64, 4096][index % 5];
        let layout = layout(size, align);
        // SAFETY: 布局有效。
        let pointer = unsafe { heap.alloc(layout) };
        assert!(!pointer.is_null());
        assert_eq!(pointer as usize % align, 0, "size {size} align {align}");
        let tag = index as u8;
        // SAFETY: 块至少 size 字节。
        unsafe { pointer.write_bytes(tag, size) };
        live.push((pointer, layout, tag));
    }
    for &(pointer, layout, tag) in &live {
        // SAFETY: 块仍存活；区间内全是写入的 tag，说明没有与其他块重叠。
        let bytes = unsafe { std::slice::from_raw_parts(pointer, layout.size()) };
        assert!(bytes.iter().all(|byte| *byte == tag));
    }
    for (pointer, layout, _) in live {
        // SAFETY: 每块恰好释放一次。
        unsafe { heap.dealloc(pointer, layout) };
    }
    assert_eq!(heap.backing.large.load(Ordering::Relaxed), 0);
}

#[test]
fn large_requests_use_independent_mappings() {
    let heap = heap();
    let layout = layout(MAX_SMALL + 1, 16);
    // SAFETY: 布局有效。
    let pointer = unsafe { heap.alloc(layout) };
    assert!(!pointer.is_null());
    assert_eq!(heap.backing.large.load(Ordering::Relaxed), 1);
    // SAFETY: 释放刚分配的块。
    unsafe { heap.dealloc(pointer, layout) };
    assert_eq!(heap.backing.large.load(Ordering::Relaxed), 0);
}

#[test]
fn realloc_grows_in_place_within_the_block_and_copies_across_classes() {
    let heap = heap();
    // SAFETY: 布局有效，块在 realloc 前后都按各自布局使用。
    unsafe {
        let original = layout(100, 8);
        let pointer = heap.alloc(original);
        pointer.write_bytes(7, 100);
        // 100 → 112 同属 112 字节级别：原地。
        let same = heap.realloc(pointer, original, 112);
        assert_eq!(same, pointer);
        let moved = heap.realloc(same, layout(112, 8), 4000);
        assert!(!moved.is_null());
        assert!(
            std::slice::from_raw_parts(moved, 100)
                .iter()
                .all(|b| *b == 7)
        );
        heap.dealloc(moved, layout(4000, 8));
    }
}

#[test]
fn an_empty_span_is_returned_and_excess_free_spans_are_decommitted() {
    let heap = heap();
    // 每个 64 KiB span 的 16 字节块有 4096 个；分配并释放跨越多个 span，使空 span 超过保留数。
    let blocks: Vec<_> = (0..4096 * (KEEP_COMMITTED as usize + 3))
        .map(|_| {
            // SAFETY: 布局有效。
            unsafe { heap.alloc(layout(16, 16)) }
        })
        .collect();
    for pointer in blocks {
        // SAFETY: 每块恰好释放一次。
        unsafe { heap.dealloc(pointer, layout(16, 16)) };
    }
    assert!(heap.backing.decommits.load(Ordering::Relaxed) > 0);
    // 回收的 span 可被再次取用而不增长地址空间。
    let before = unsafe { *heap.shared.bump_unit.get() };
    let again: Vec<_> = (0..4096)
        .map(|_| unsafe { heap.alloc(layout(16, 16)) })
        .collect();
    assert_eq!(unsafe { *heap.shared.bump_unit.get() }, before);
    for pointer in again {
        // SAFETY: 每块恰好释放一次。
        unsafe { heap.dealloc(pointer, layout(16, 16)) };
    }
}

#[test]
fn blocks_freed_by_other_threads_return_to_the_owner_without_loss() {
    let heap = heap();
    const ROUNDS: usize = 20;
    const BATCH: usize = 5000;
    let (sender, receiver) = std::sync::mpsc::channel::<Vec<usize>>();
    let consumer = {
        let heap = heap.clone();
        std::thread::spawn(move || {
            let mut freed = 0usize;
            while let Ok(batch) = receiver.recv() {
                for address in batch {
                    // SAFETY: 生产者分配的 24 字节块，这里恰好释放一次（远端释放路径）。
                    unsafe { heap.dealloc(address as *mut u8, layout(24, 8)) };
                    freed += 1;
                }
            }
            freed
        })
    };
    let mut seen = std::collections::HashSet::new();
    for _ in 0..ROUNDS {
        let batch: Vec<usize> = (0..BATCH)
            .map(|_| {
                // SAFETY: 布局有效。
                let pointer = unsafe { heap.alloc(layout(24, 8)) } as usize;
                assert_ne!(pointer, 0);
                pointer
            })
            .collect();
        // 同一时刻存活的块地址互不相同（远端释放没有把仍在使用的块交还）。
        let live: std::collections::HashSet<_> = batch.iter().copied().collect();
        assert_eq!(live.len(), BATCH);
        seen.extend(batch.iter().copied());
        sender.send(batch).unwrap();
    }
    drop(sender);
    assert_eq!(consumer.join().unwrap(), ROUNDS * BATCH);
    // 远端释放的块被所属线程收回复用：地址总数远小于总分配数。
    assert!(
        seen.len() < ROUNDS * BATCH,
        "remote frees were never reused"
    );
}

#[test]
fn spans_of_an_exited_thread_are_adopted_and_its_empty_spans_are_returned() {
    let heap = heap();
    let kept: Vec<usize> = {
        let heap = heap.clone();
        std::thread::spawn(move || {
            let live: Vec<usize> = (0..100)
                .map(|_| unsafe { heap.alloc(layout(48, 16)) } as usize)
                .collect();
            let garbage = unsafe { heap.alloc(layout(2048, 16)) };
            unsafe { heap.dealloc(garbage, layout(2048, 16)) };
            heap.retire_thread(slot());
            live
        })
        .join()
        .unwrap()
    };
    // 孤儿 span 里仍有存活块：本线程释放它们（远端路径），再分配时应认领并复用该 span。
    for &address in &kept {
        // SAFETY: 每块恰好释放一次。
        unsafe { heap.dealloc(address as *mut u8, layout(48, 16)) };
    }
    let reused: Vec<usize> = (0..100)
        .map(|_| unsafe { heap.alloc(layout(48, 16)) } as usize)
        .collect();
    let kept_set: std::collections::HashSet<_> = kept.iter().copied().collect();
    assert!(reused.iter().all(|address| kept_set.contains(address)));
    for address in reused {
        unsafe { heap.dealloc(address as *mut u8, layout(48, 16)) };
    }
}

#[test]
fn concurrent_mixed_workload_never_hands_out_a_live_block_twice() {
    let heap = heap();
    let threads: Vec<_> = (0..4)
        .map(|worker| {
            let heap = heap.clone();
            std::thread::spawn(move || {
                let mut live: Vec<(usize, usize, u8)> = Vec::new();
                let mut state = 0x1234_5678u64 + worker as u64;
                for step in 0..20_000usize {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let size = 1 + (state >> 33) as usize % 3000;
                    if live.len() > 200 || (state >> 20) & 1 == 0 && !live.is_empty() {
                        let index = (state >> 40) as usize % live.len();
                        let (address, size, tag) = live.swap_remove(index);
                        // SAFETY: 块仍存活，内容完整。
                        let bytes =
                            unsafe { std::slice::from_raw_parts(address as *const u8, size) };
                        assert!(bytes.iter().all(|byte| *byte == tag), "block corrupted");
                        unsafe { heap.dealloc(address as *mut u8, layout(size, 8)) };
                    } else {
                        let pointer = unsafe { heap.alloc(layout(size, 8)) };
                        assert!(!pointer.is_null());
                        let tag = (step as u8) ^ worker as u8;
                        unsafe { pointer.write_bytes(tag, size) };
                        live.push((pointer as usize, size, tag));
                    }
                }
                for (address, size, tag) in live {
                    let bytes = unsafe { std::slice::from_raw_parts(address as *const u8, size) };
                    assert!(bytes.iter().all(|byte| *byte == tag));
                    unsafe { heap.dealloc(address as *mut u8, layout(size, 8)) };
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
}
