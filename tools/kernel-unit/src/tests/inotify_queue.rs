//! inotify 事件队列：合并、溢出与线格式。

use super::inotify_queue::{
    BufferTooSmall, Event, EventQueue, IN_Q_OVERFLOW, MAX_QUEUED_EVENTS, PreparedEvent, Pushed,
};
use std::{boxed::Box, vec::Vec};

fn event(wd: i32, mask: u32, name: &[u8]) -> Event {
    Event {
        wd,
        mask,
        cookie: 0,
        name: Box::from(name),
    }
}

// fixture 只生产锁外 storage 与 codec buffer；队列/合并/overflow/退休全部运行 production owner。
fn push(queue: &mut EventQueue, event: Event) -> Pushed {
    let prepared: PreparedEvent = EventQueue::prepare(event).unwrap();
    let (outcome, rejected) = queue.push(Some(prepared));
    drop(rejected);
    outcome
}
fn encode(queue: &mut EventQueue, capacity: usize) -> Result<(Vec<u8>, usize), BufferTooSmall> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity.min(queue.pending_bytes()))
        .unwrap();
    let mut count = 0;
    loop {
        let record = match queue.pop(capacity - bytes.len()) {
            Ok(Some(record)) => record,
            Ok(None) => break,
            Err(error) if count == 0 => return Err(error),
            Err(_) => break,
        };
        record
            .copy_with(|part| {
                bytes.extend_from_slice(part);
                Ok::<_, ()>(())
            })
            .unwrap();
        count += 1;
    }
    Ok((bytes, count))
}
fn discard(queue: &mut EventQueue, count: usize) {
    for _ in 0..count {
        drop(queue.pop(usize::MAX).unwrap().unwrap());
    }
}

#[test]
fn adjacent_identical_events_coalesce_but_distinct_or_non_adjacent_do_not() {
    let mut queue = EventQueue::try_new(MAX_QUEUED_EVENTS).unwrap();
    assert_eq!(push(&mut queue, event(1, 2, b"a")), Pushed::Queued);
    assert_eq!(push(&mut queue, event(1, 2, b"a")), Pushed::Coalesced);
    assert_eq!(push(&mut queue, event(1, 2, b"b")), Pushed::Queued);
    assert_eq!(push(&mut queue, event(1, 2, b"a")), Pushed::Queued);
    let (bytes, count) = encode(&mut queue, 4096).unwrap();
    assert_eq!(count, 3);
    assert_eq!(bytes.len(), 3 * 32);
}

#[test]
fn wire_format_pads_the_name_to_sixteen_bytes_with_nul() {
    let mut queue = EventQueue::try_new(MAX_QUEUED_EVENTS).unwrap();
    push(
        &mut queue,
        Event {
            wd: 7,
            mask: 0x100,
            cookie: 9,
            name: Box::from(&b"file"[..]),
        },
    );
    push(&mut queue, event(7, 0x8000, b""));
    let (bytes, count) = encode(&mut queue, 4096).unwrap();
    assert_eq!(count, 2);
    assert_eq!(&bytes[0..4], &7i32.to_ne_bytes());
    assert_eq!(&bytes[4..8], &0x100u32.to_ne_bytes());
    assert_eq!(&bytes[8..12], &9u32.to_ne_bytes());
    assert_eq!(&bytes[12..16], &16u32.to_ne_bytes()); // "file\0" 对齐到 16
    assert_eq!(&bytes[16..21], b"file\0");
    assert!(bytes[21..32].iter().all(|byte| *byte == 0));
    // 没有名字的事件只有头部，len 为 0。
    assert_eq!(&bytes[44..48], &0u32.to_ne_bytes());
    assert_eq!(bytes.len(), 48);
    // 恰好 16 字节的名字也需要结尾 NUL，所以占 32 字节。
    assert_eq!(event(1, 1, &[b'x'; 16]).wire_len(), 16 + 32);
    assert_eq!(event(1, 1, &[b'x'; 15]).wire_len(), 16 + 16);
}

#[test]
fn encoding_stops_at_the_last_whole_event_and_reports_a_too_small_buffer() {
    let mut queue = EventQueue::try_new(MAX_QUEUED_EVENTS).unwrap();
    push(&mut queue, event(1, 1, b"a"));
    push(&mut queue, event(1, 2, b"b"));
    assert_eq!(encode(&mut queue, 31), Err(BufferTooSmall));
    let (bytes, count) = encode(&mut queue, 40).unwrap();
    assert_eq!((count, bytes.len()), (1, 32));
    // 单个记录在 copy 前已出队；不足容纳第二条记录时保留它。
    assert_eq!(queue.pending_bytes(), 32);
    discard(&mut queue, 1);
    assert!(queue.is_empty());
    assert_eq!(encode(&mut queue, 8), Ok((Vec::new(), 0)));
}

#[test]
fn a_full_queue_drops_events_and_keeps_exactly_one_trailing_overflow() {
    let mut queue = EventQueue::try_new(MAX_QUEUED_EVENTS).unwrap();
    for index in 0..MAX_QUEUED_EVENTS {
        // 实际 wd 不同，避免被合并；native merge 不比较 rename cookie。
        push(
            &mut queue,
            Event {
                wd: index as i32 + 1,
                mask: 1,
                cookie: index as u32,
                name: Box::default(),
            },
        );
    }
    assert_eq!(
        push(&mut queue, event(1, 2, b"dropped")),
        Pushed::Overflowed
    );
    assert_eq!(
        push(&mut queue, event(1, 4, b"dropped")),
        Pushed::Overflowed
    );
    let (bytes, count) = encode(&mut queue, usize::MAX).unwrap();
    assert_eq!(count, MAX_QUEUED_EVENTS + 1);
    let last = &bytes[bytes.len() - 16..];
    assert_eq!(&last[0..4], &(-1i32).to_ne_bytes());
    assert_eq!(&last[4..8], &IN_Q_OVERFLOW.to_ne_bytes());
}

#[test]
fn failed_node_allocation_uses_reusable_overflow_record_without_queue_growth() {
    let mut queue = EventQueue::try_new(MAX_QUEUED_EVENTS).unwrap();
    for _ in 0..3 {
        let (outcome, rejected) = queue.push(None);
        assert_eq!(outcome, Pushed::Overflowed);
        assert!(rejected.is_none());
        assert_eq!(queue.pending_bytes(), 16);
        let (bytes, count) = encode(&mut queue, 16).unwrap();
        assert_eq!(count, 1);
        assert_eq!(&bytes[4..8], &IN_Q_OVERFLOW.to_ne_bytes());
        assert!(queue.is_empty());
    }
}

#[test]
fn overflow_stays_unique_after_partial_read_and_refill_before_it_is_consumed() {
    let mut queue = EventQueue::try_new(MAX_QUEUED_EVENTS).unwrap();
    for cookie in 0..MAX_QUEUED_EVENTS {
        push(
            &mut queue,
            Event {
                wd: cookie as i32 + 1,
                mask: 1,
                cookie: cookie as u32,
                name: Box::default(),
            },
        );
    }
    drop(queue.push(None));
    discard(&mut queue, MAX_QUEUED_EVENTS);
    for cookie in 0..MAX_QUEUED_EVENTS {
        push(
            &mut queue,
            Event {
                wd: cookie as i32 + 1,
                mask: 2,
                cookie: cookie as u32,
                name: Box::default(),
            },
        );
    }
    drop(queue.push(None));
    let (bytes, count) = encode(&mut queue, usize::MAX).unwrap();
    assert_eq!(count, MAX_QUEUED_EVENTS);
    assert_eq!(
        bytes
            .as_chunks::<16>()
            .0
            .iter()
            .filter(|event| event[4..8] == IN_Q_OVERFLOW.to_ne_bytes())
            .count(),
        1
    );
}

#[test]
fn native_merge_ignores_cookie_but_never_merges_ignored_or_full_queue_events() {
    let mut queue = EventQueue::try_new(4).unwrap();
    let mut first = event(1, 0x40, b"rename");
    first.cookie = 1;
    assert_eq!(push(&mut queue, first.clone()), Pushed::Queued);
    first.cookie = 2;
    assert_eq!(push(&mut queue, first), Pushed::Coalesced);
    assert_eq!(push(&mut queue, event(1, 0x8000, b"")), Pushed::Queued);
    assert_eq!(push(&mut queue, event(1, 0x8000, b"")), Pushed::Queued);
    assert_eq!(push(&mut queue, event(1, 2, b"")), Pushed::Queued);
    assert_eq!(push(&mut queue, event(1, 2, b"")), Pushed::Overflowed);
    assert_eq!(encode(&mut queue, usize::MAX).unwrap().1, 5);
}
