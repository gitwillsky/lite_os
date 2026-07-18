use alloc::sync::Arc;

use crate::file_position::FilePosition;

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
                    position.with(|value| *value += 1);
                }
            });
        }
    });

    assert_eq!(position.snapshot(), (THREADS * UPDATES) as u64);
}

#[test]
fn pair_locking_preserves_caller_order_in_both_address_orders() {
    let first = FilePosition::new();
    let second = FilePosition::new();
    first.with(|value| *value = 3);
    second.with(|value| *value = 7);

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

    assert_eq!(first.snapshot(), 213);
    assert_eq!(second.snapshot(), 127);
    assert!(FilePosition::with_pair(&first, &first, |_, _| ()).is_none());
}

#[test]
fn seek_rejects_values_outside_signed_loff_t_without_publishing_them() {
    let position = FilePosition::new();

    assert_eq!(position.seek(-1, |_| 0), Err(()));
    assert_eq!(position.snapshot(), 0);
    assert_eq!(
        position.seek(-1, |_| i64::MAX as u64),
        Ok(i64::MAX as u64 - 1)
    );
    assert_eq!(position.seek(2, |current| current), Err(()));
    assert_eq!(position.snapshot(), i64::MAX as u64 - 1);
}
