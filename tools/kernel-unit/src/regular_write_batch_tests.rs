use core::cell::Cell;

use crate::{
    regular_write_policy::{regular_write_allowance, regular_write_chunk},
    user_iovec::{UserIoCursor, UserIoVec, fallible_staging_capacity, with_prepared_staging},
    writeback_batch::{WRITEBACK_BATCH_PAGES, commit_contiguous_prefix_with_backoff},
};

const PAGE_SIZE: usize = 4096;
const STAGING_BYTES: usize = WRITEBACK_BATCH_PAGES * PAGE_SIZE;
const ONE_MIB: usize = 1024 * 1024;
const FLUSHES_PER_JOURNAL_TRANSACTION: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendError {
    Capacity,
    Io,
}

#[test]
fn small_write_uses_stack_and_large_staging_oom_falls_back_to_one_page() {
    assert_eq!(fallible_staging_capacity(1, PAGE_SIZE, false), 1);
    assert_eq!(
        fallible_staging_capacity(PAGE_SIZE, PAGE_SIZE, false),
        PAGE_SIZE
    );
    assert_eq!(
        fallible_staging_capacity(STAGING_BYTES, PAGE_SIZE, false),
        PAGE_SIZE
    );
    assert_eq!(
        fallible_staging_capacity(STAGING_BYTES, PAGE_SIZE, true),
        128 * 1024
    );
}

#[test]
fn staging_prepare_and_drop_happen_outside_the_operation_gate() {
    struct DropProbe<'a>(&'a Cell<u8>);

    impl Drop for DropProbe<'_> {
        fn drop(&mut self) {
            assert_eq!(self.0.get(), 2, "staging dropped before gate exit");
            self.0.set(3);
        }
    }

    fn prepare(state: &Cell<u8>) -> DropProbe<'_> {
        assert_eq!(state.get(), 0);
        state.set(1);
        DropProbe(state)
    }

    let state = Cell::new(0);
    let result = with_prepared_staging(prepare(&state), |_| {
        assert_eq!(state.get(), 1, "gate entered before staging preparation");
        state.set(2);
        17
    });
    assert_eq!(result, 17);
    assert_eq!(state.get(), 3, "staging did not drop after gate exit");
}

#[derive(Debug, Default, PartialEq, Eq)]
struct BackendCounters {
    attempts: usize,
    transactions: usize,
    flushes: usize,
    published: usize,
    maximum_physical_pages: usize,
}

fn physical_pages(offset: u64, bytes: usize) -> usize {
    (offset as usize % PAGE_SIZE + bytes).div_ceil(PAGE_SIZE)
}

fn run_sequential(
    initial_offset: u64,
    bytes: usize,
    physical_page_capacity: usize,
) -> BackendCounters {
    let mut attempts = 0;
    let mut transactions = 0;
    let mut flushes = 0;
    let mut published = 0;
    let mut maximum_physical_pages = 0;
    let mut completed = 0;
    while completed < bytes {
        let window = (bytes - completed).min(STAGING_BYTES);
        let window_offset = initial_offset + completed as u64;
        let committed = commit_contiguous_prefix_with_backoff(
            window,
            PAGE_SIZE,
            |start, count| {
                attempts += 1;
                let offset = window_offset + start as u64;
                let pages = physical_pages(offset, count);
                maximum_physical_pages = maximum_physical_pages.max(pages);
                if pages > physical_page_capacity {
                    return Err(BackendError::Capacity);
                }
                transactions += 1;
                flushes += FLUSHES_PER_JOURNAL_TRANSACTION;
                Ok((offset, count))
            },
            |offset, start, count| {
                assert_eq!(offset, window_offset + start as u64);
                published += count;
            },
            |error| *error == BackendError::Capacity,
        )
        .unwrap();
        assert_eq!(committed.offset, window_offset);
        assert_eq!(committed.bytes, window);
        completed += committed.bytes;
    }
    BackendCounters {
        attempts,
        transactions,
        flushes,
        published,
        maximum_physical_pages,
    }
}

#[test]
fn one_mib_aligned_sequential_write_reduces_transactions_and_flushes_32x() {
    assert_eq!(STAGING_BYTES, 128 * 1024);
    let legacy_transactions = ONE_MIB / PAGE_SIZE;
    let legacy_flushes = legacy_transactions * FLUSHES_PER_JOURNAL_TRANSACTION;
    assert_eq!((legacy_transactions, legacy_flushes), (256, 1024));

    let counters = run_sequential(0, ONE_MIB, WRITEBACK_BATCH_PAGES);
    assert_eq!(
        counters,
        BackendCounters {
            attempts: 8,
            transactions: 8,
            flushes: 32,
            published: ONE_MIB,
            maximum_physical_pages: 32,
        }
    );
}

#[test]
fn unaligned_128k_spans_33_pages_and_backs_off_without_publication() {
    let counters = run_sequential(1, ONE_MIB, 32);
    // 每个 128 KiB window 首次覆盖 33 pages 而失败，再以两个 64 KiB/17-page transaction 提交。
    assert_eq!(counters.maximum_physical_pages, 33);
    assert_eq!(counters.attempts, 24);
    assert_eq!(counters.transactions, 16);
    assert_eq!(counters.flushes, 64);
    assert_eq!(counters.published, ONE_MIB);

    let fitting = run_sequential(1, ONE_MIB, 33);
    assert_eq!((fitting.transactions, fitting.flushes), (8, 32));
}

#[test]
fn faulted_user_prefix_is_not_advanced_until_backend_publication() {
    let vectors = [UserIoVec {
        base: PAGE_SIZE,
        length: 2 * PAGE_SIZE,
    }];
    let cursor = UserIoCursor::new(&vectors);
    let mut staging = [0u8; 2 * PAGE_SIZE];
    let mut copy_calls = 0;
    let staged = cursor.stage_pagewise_with(&mut staging, |address, output| {
        copy_calls += 1;
        assert!(output.len() <= PAGE_SIZE);
        if address == 2 * PAGE_SIZE {
            return Err(());
        }
        output.fill(7);
        Ok(())
    });
    assert_eq!((staged.count, staged.faulted), (PAGE_SIZE, true));
    assert_eq!(copy_calls, 2);
    assert_eq!(cursor.completed(), 0);

    let mut cursor = cursor;
    cursor.advance(staged.count);
    assert_eq!(cursor.completed(), PAGE_SIZE);
}

#[test]
fn rlimit_and_append_offsets_bound_every_committed_batch() {
    let offset = 3 * PAGE_SIZE as u64;
    let limit = offset + 10 * PAGE_SIZE as u64 + 123;
    let allowed = regular_write_allowance(offset, limit, STAGING_BYTES);
    assert_eq!(allowed, 10 * PAGE_SIZE + 123);
    assert_eq!(regular_write_allowance(limit, limit, STAGING_BYTES), 0);

    let append_size = Cell::new(offset);
    let published_end = Cell::new(offset);
    let attempts = Cell::new(0);
    let committed = commit_contiguous_prefix_with_backoff(
        allowed,
        PAGE_SIZE,
        |_, count| {
            attempts.set(attempts.get() + 1);
            if count > 4 * PAGE_SIZE {
                return Err(BackendError::Capacity);
            }
            let start = append_size.get();
            append_size.set(start + count as u64);
            Ok((start, count))
        },
        |storage_offset, _, count| {
            assert_eq!(storage_offset, published_end.get());
            published_end.set(storage_offset + count as u64);
        },
        |error| *error == BackendError::Capacity,
    )
    .unwrap();
    assert_eq!(committed.offset, offset);
    assert_eq!(committed.bytes, allowed);
    assert_eq!(append_size.get(), limit);
    assert_eq!(published_end.get(), limit);
    assert_eq!(attempts.get(), 6);
}

#[test]
fn exact_limit_completion_has_no_followup_allowance_iteration() {
    let total_length = 2 * PAGE_SIZE;
    let limit = total_length as u64;
    let mut offset = 0u64;
    let mut completed = 0usize;
    let mut allowance_calls = 0;
    while completed < total_length {
        let requested = regular_write_chunk(total_length, completed, PAGE_SIZE);
        allowance_calls += 1;
        let allowed = regular_write_allowance(offset, limit, requested);
        assert_eq!(allowed, requested);
        completed += allowed;
        offset += allowed as u64;
    }
    assert_eq!(completed, total_length);
    assert_eq!(offset, limit);
    assert_eq!(allowance_calls, 2);
    assert_eq!(regular_write_chunk(total_length, completed, PAGE_SIZE), 0);
}

#[test]
fn final_staging_chunk_crosses_iovec_boundary_without_overrun() {
    let vectors = [
        UserIoVec {
            base: PAGE_SIZE,
            length: 200,
        },
        UserIoVec {
            base: 2 * PAGE_SIZE,
            length: 100,
        },
    ];
    let total_length = 300;
    let mut cursor = UserIoCursor::new(&vectors);
    let mut staging = [0u8; 256];
    let mut chunks = [0usize; 2];
    let mut chunk_count = 0;
    let mut completed = 0;
    while completed < total_length {
        let requested = regular_write_chunk(total_length, completed, staging.len());
        let staged = cursor.stage_pagewise_with(&mut staging[..requested], |_, output| {
            output.fill(1);
            Ok(())
        });
        assert!(!staged.faulted);
        assert_eq!(staged.count, requested);
        cursor.advance(staged.count);
        chunks[chunk_count] = staged.count;
        chunk_count += 1;
        completed += staged.count;
    }
    assert_eq!(chunks, [256, 44]);
    assert_eq!(cursor.completed(), total_length);
    assert_eq!(
        regular_write_chunk(total_length, completed, staging.len()),
        0
    );
}

#[test]
fn short_write_and_later_io_error_return_only_durable_prefix() {
    let published = Cell::new(0);
    let committed = commit_contiguous_prefix_with_backoff(
        3 * PAGE_SIZE,
        PAGE_SIZE,
        |start, count| {
            if start == PAGE_SIZE {
                return Err(BackendError::Io);
            }
            if count > PAGE_SIZE {
                return Err(BackendError::Capacity);
            }
            Ok((start as u64, count))
        },
        |_, _, count| published.set(published.get() + count),
        |error| *error == BackendError::Capacity,
    )
    .unwrap();
    assert_eq!(committed.bytes, PAGE_SIZE);
    assert_eq!(published.get(), PAGE_SIZE);

    published.set(0);
    let committed = commit_contiguous_prefix_with_backoff(
        3 * PAGE_SIZE,
        PAGE_SIZE,
        |start, count| Ok::<_, BackendError>((start as u64, count.min(PAGE_SIZE + 17))),
        |_, _, count| published.set(published.get() + count),
        |_| false,
    )
    .unwrap();
    assert_eq!(committed.bytes, PAGE_SIZE + 17);
    assert_eq!(published.get(), PAGE_SIZE + 17);
}
