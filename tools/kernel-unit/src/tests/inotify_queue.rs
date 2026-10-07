//! inotify 事件队列：合并、溢出与线格式。

use super::inotify_queue::{Event, EventQueue, IN_Q_OVERFLOW, MAX_QUEUED_EVENTS, Pushed};
use std::{boxed::Box, vec::Vec};

fn event(wd: i32, mask: u32, name: &[u8]) -> Event {
    Event {
        wd,
        mask,
        cookie: 0,
        name: Box::from(name),
    }
}

#[test]
fn adjacent_identical_events_coalesce_but_distinct_or_non_adjacent_do_not() {
    let mut queue = EventQueue::new();
    assert_eq!(queue.push(event(1, 2, b"a")), Pushed::Queued);
    assert_eq!(queue.push(event(1, 2, b"a")), Pushed::Coalesced);
    assert_eq!(queue.push(event(1, 2, b"b")), Pushed::Queued);
    assert_eq!(queue.push(event(1, 2, b"a")), Pushed::Queued);
    let (bytes, count) = queue.encode_prefix(4096).unwrap();
    assert_eq!(count, 3);
    assert_eq!(bytes.len(), 3 * 32);
}

#[test]
fn wire_format_pads_the_name_to_sixteen_bytes_with_nul() {
    let mut queue = EventQueue::new();
    queue.push(Event {
        wd: 7,
        mask: 0x100,
        cookie: 9,
        name: Box::from(&b"file"[..]),
    });
    queue.push(event(7, 0x8000, b""));
    let (bytes, count) = queue.encode_prefix(4096).unwrap();
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
    let mut queue = EventQueue::new();
    queue.push(event(1, 1, b"a"));
    queue.push(event(1, 2, b"b"));
    assert_eq!(queue.encode_prefix(31), Err(()));
    let (bytes, count) = queue.encode_prefix(40).unwrap();
    assert_eq!((count, bytes.len()), (1, 32));
    assert_eq!(queue.pending_bytes(), 64);
    // 编码不出队；交付成功后才丢弃。
    queue.discard(1);
    assert_eq!(queue.pending_bytes(), 32);
    queue.discard(1);
    assert!(queue.is_empty());
    assert_eq!(queue.encode_prefix(8), Ok((Vec::new(), 0)));
}

#[test]
fn a_full_queue_drops_events_and_keeps_exactly_one_trailing_overflow() {
    let mut queue = EventQueue::new();
    for index in 0..MAX_QUEUED_EVENTS {
        // 每个事件的 cookie 不同，避免被合并。
        queue.push(Event {
            wd: 1,
            mask: 1,
            cookie: index as u32,
            name: Box::default(),
        });
    }
    assert_eq!(queue.push(event(1, 2, b"dropped")), Pushed::Overflowed);
    assert_eq!(queue.push(event(1, 4, b"dropped")), Pushed::Overflowed);
    let (bytes, count) = queue.encode_prefix(usize::MAX).unwrap();
    assert_eq!(count, MAX_QUEUED_EVENTS + 1);
    let last = &bytes[bytes.len() - 16..];
    assert_eq!(&last[0..4], &(-1i32).to_ne_bytes());
    assert_eq!(&last[4..8], &IN_Q_OVERFLOW.to_ne_bytes());
}
