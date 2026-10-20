use alloc::sync::Arc;

use crate::file_position::{FilePosition, PositionError};

#[test]
fn try_with_does_not_wait_for_an_occupied_position() {
    let position = FilePosition::new();
    position.with(|value| *value = 7).unwrap();
    assert_eq!(position.try_with(|value| *value), Some(7));
    // 占用期间（同步读持有 position 做 I/O）不能等待：io_uring reactor 依赖它立即得到 `None`。
    position
        .with(|_| assert_eq!(position.try_with(|value| *value), None))
        .unwrap();
    assert_eq!(position.try_with(|value| *value), Some(7));
}

#[test]
fn shared_position_updates_do_not_lose_concurrent_progress() {
    const THREADS: usize = 4;
    const UPDATES: usize = 10_000;
    let position = Arc::new(FilePosition::new());

    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            let position = position.clone();
            scope.spawn(move || {
                for _ in 0..UPDATES {
                    position.with(|value| *value += 1).unwrap();
                }
            });
        }
    });

    assert_eq!(position.snapshot().unwrap(), (THREADS * UPDATES) as u64);
}

#[test]
fn pair_locking_preserves_caller_order_in_both_address_orders() {
    let first = FilePosition::new();
    let second = FilePosition::new();
    first.with(|value| *value = 3).unwrap();
    second.with(|value| *value = 7).unwrap();

    FilePosition::with_pair(&first, &second, |first, second| {
        *first += 10;
        *second += 20;
    })
    .unwrap();
    FilePosition::with_pair(&second, &first, |second, first| {
        *second += 100;
        *first += 200;
    })
    .unwrap();

    assert_eq!(first.snapshot().unwrap(), 213);
    assert_eq!(second.snapshot().unwrap(), 127);
    assert_eq!(
        FilePosition::with_pair(&first, &first, |_, _| ()),
        Err(PositionError::SameOwner)
    );
}

#[test]
fn seek_rejects_values_outside_signed_loff_t_without_publishing_them() {
    let position = FilePosition::new();

    assert_eq!(position.seek(-1, |_| 0), Err(PositionError::InvalidSeek));
    assert_eq!(position.snapshot().unwrap(), 0);
    assert_eq!(
        position.seek(-1, |_| i64::MAX as u64),
        Ok(i64::MAX as u64 - 1)
    );
    assert_eq!(
        position.seek(2, |current| current),
        Err(PositionError::InvalidSeek)
    );
    assert_eq!(position.snapshot().unwrap(), i64::MAX as u64 - 1);
}

#[test]
fn contended_position_oom_never_enters_io_or_changes_offset() {
    let position = Arc::new(FilePosition::new());
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let owner_position = position.clone();
    let owner = std::thread::spawn(move || {
        owner_position
            .with(|value| {
                *value = 17;
                held_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .unwrap();
    });
    held_rx.recv().unwrap();
    let mut entered = false;
    crate::allocation_failure::fail_next();
    let result = position.with(|_| entered = true);
    let consumed = crate::allocation_failure::was_consumed();
    assert!(consumed);
    assert_eq!(result, Err(PositionError::OutOfMemory));
    assert!(!entered);
    release_tx.send(()).unwrap();
    owner.join().unwrap();
    assert_eq!(position.snapshot().unwrap(), 17);
}

#[test]
fn second_position_acquisition_oom_releases_first_without_half_publication() {
    let positions = Arc::new([FilePosition::new(), FilePosition::new()]);
    // Array address order is stable while Arc lives; index 0 must be acquired before index 1.
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let owner_positions = positions.clone();
    let owner = std::thread::spawn(move || {
        owner_positions[1]
            .with(|value| {
                *value = 31;
                held_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .unwrap();
    });
    held_rx.recv().unwrap();
    let mut entered = false;
    crate::allocation_failure::fail_next();
    let result = FilePosition::with_pair(&positions[1], &positions[0], |_, _| entered = true);
    let consumed = crate::allocation_failure::was_consumed();
    assert!(consumed);
    assert_eq!(result, Err(PositionError::OutOfMemory));
    assert!(!entered);
    // This acquisition would block forever if the failed pair retained its first guard.
    assert_eq!(positions[0].snapshot().unwrap(), 0);
    release_tx.send(()).unwrap();
    owner.join().unwrap();
    assert_eq!(positions[1].snapshot().unwrap(), 31);
}

#[test]
#[ignore = "CPU diagnostic of actual uncontended sleeping OFD owner; run once during consolidated verification"]
fn actual_file_position_uncontended_cpu_cost() {
    let position = FilePosition::new();
    for _ in 0..100_000 {
        std::hint::black_box(position.snapshot().unwrap());
    }
    let mut samples = [0f64; 5];
    for sample in &mut samples {
        let start = std::time::Instant::now();
        for _ in 0..1_000_000 {
            std::hint::black_box(
                position
                    .with(|offset| {
                        *offset += 1;
                        *offset
                    })
                    .unwrap(),
            );
        }
        *sample = start.elapsed().as_nanos() as f64 / 1_000_000.0;
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "actual sleeping OFD position: {:.2} ns/op; owner={} bytes",
        samples[2],
        core::mem::size_of::<FilePosition>()
    );
}
