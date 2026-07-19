use crate::user_iovec::{UserIoCursor, UserIoVec};

#[test]
fn scalar_zero_uses_one_contiguous_transaction() {
    let vectors = [UserIoVec {
        base: 0x1000,
        length: 1024 * 1024,
    }];
    let mut calls = Vec::new();
    let mut cursor = UserIoCursor::new(&vectors);
    assert_eq!(
        cursor.zero_with(|address, length| {
            calls.push((address, length));
            Ok(())
        }),
        Ok(1024 * 1024)
    );
    assert_eq!(calls, [(0x1000, 1024 * 1024)]);
    assert_eq!(cursor.completed(), 1024 * 1024);
}

#[test]
fn zero_fault_preserves_only_completed_vector_progress() {
    let vectors = [
        UserIoVec {
            base: 0x2000,
            length: 64,
        },
        UserIoVec {
            base: 0x3000,
            length: 128,
        },
    ];
    let mut calls = 0;
    let mut cursor = UserIoCursor::new(&vectors);
    assert_eq!(
        cursor.zero_with(|_, _| {
            calls += 1;
            (calls == 1).then_some(()).ok_or(())
        }),
        Err(())
    );
    assert_eq!(calls, 2);
    assert_eq!(cursor.completed(), 64);
}
