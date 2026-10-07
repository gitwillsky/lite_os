//! `/dev/kmsg` 的 record 线格式与用户写入前缀解析。

use crate::kmsg_wire::{Encoded, encode, parse_user_message};
use std::vec::Vec;

fn collect(priority: u8, message: &[u8], capacity: usize) -> (Encoded, Vec<u8>) {
    let mut out = Vec::new();
    let result = encode::<()>(priority, 7, 1234, message, capacity, &mut |bytes| {
        out.extend_from_slice(bytes);
        Ok(())
    })
    .unwrap();
    (result, out)
}

#[test]
fn record_has_linux_header_and_trailing_newline() {
    let (result, wire) = collect(6, b"hello", 64);
    assert_eq!(result, Encoded::Done);
    assert_eq!(wire, b"6,7,1234,-;hello\n");
}

#[test]
fn control_backslash_and_non_ascii_bytes_are_escaped_so_a_record_stays_one_line() {
    let (_, wire) = collect(8, b"a\nb\\c\xc3\xa9", 128);
    assert_eq!(wire, b"8,7,1234,-;a\\x0ab\\x5cc\\xc3\\xa9\n");
    assert_eq!(wire.iter().filter(|byte| **byte == b'\n').count(), 1);
}

#[test]
fn a_record_is_delivered_whole_or_not_at_all() {
    let full = b"6,7,1234,-;hello\n".len();
    let (result, wire) = collect(6, b"hello", full - 1);
    assert_eq!(result, Encoded::TooSmall);
    assert!(wire.is_empty());
    assert_eq!(collect(6, b"hello", full).0, Encoded::Done);
}

#[test]
fn long_escaped_messages_cross_chunk_boundaries_intact() {
    let message = std::vec![b'\n'; 1024];
    let (result, wire) = collect(0, &message, 8192);
    assert_eq!(result, Encoded::Done);
    let body = &wire[b"0,7,1234,-;".len()..wire.len() - 1];
    assert_eq!(body.len(), 1024 * 4);
    assert!(body.chunks(4).all(|escape| escape == b"\\x0a"));
}

#[test]
fn emit_failure_aborts_encoding() {
    let mut calls = 0;
    let result = encode(6, 1, 1, b"x", 64, &mut |_| {
        calls += 1;
        Err("fault")
    });
    assert_eq!(result, Err("fault"));
    assert_eq!(calls, 1);
}

#[test]
fn user_prefix_selects_facility_and_level_and_newline_is_stripped() {
    assert_eq!(
        parse_user_message(b"<6>boot ok\n", 4),
        (1 << 3 | 6, &b"boot ok"[..])
    );
    // facility 3 (daemon), level 2.
    assert_eq!(parse_user_message(b"<26>x", 4), (3 << 3 | 2, &b"x"[..]));
    // 没有前缀：LOG_USER 与缺省 level。
    assert_eq!(parse_user_message(b"plain", 4), (1 << 3 | 4, &b"plain"[..]));
    // 非法前缀按普通文本处理。
    assert_eq!(parse_user_message(b"<x>y", 4), (1 << 3 | 4, &b"<x>y"[..]));
    assert_eq!(parse_user_message(b"<>y", 4), (1 << 3 | 4, &b"<>y"[..]));
    // 只去掉一个结尾换行。
    assert_eq!(parse_user_message(b"a\n\n", 4).1, b"a\n");
}
