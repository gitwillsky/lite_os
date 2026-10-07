//! `/dev/kmsg` 的 record 线格式（Linux `devkmsg_read` 的 text 输出）：`<pri>,<seq>,<us>,-;<msg>\n`。
//!
//! 消息体中的控制字符、`\` 与非 ASCII 字节转义为 `\xNN`（Linux `msg_print_ext_body`），所以
//! 用户写入的任意字节都不会破坏“一行一条 record”的框架。纯函数，不触碰 ring。

use core::fmt::{self, Write};

/// 一条 record 的 header 上限：`255,<u64>,<u64>,-;`。
const HEADER_CAPACITY: usize = 64;
/// 每次交给 `emit` 的消息体分片上限；转义在这个栈缓冲里完成，不分配。
const CHUNK: usize = 256;

struct Header {
    bytes: [u8; HEADER_CAPACITY],
    length: usize,
}

impl Write for Header {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.length.checked_add(text.len()).ok_or(fmt::Error)?;
        self.bytes
            .get_mut(self.length..end)
            .ok_or(fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}

fn escapes(byte: u8) -> bool {
    !(b' '..127).contains(&byte) || byte == b'\\'
}

/// 编码结果。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Encoded {
    /// 整条 record 已交给 `emit`。
    Done,
    /// `capacity` 不足以容纳整条 record；没有任何字节被交出。
    TooSmall,
}

/// 把一条 record 编码并交给 `emit`。
///
/// 先算出完整长度并与 `capacity` 比较，之后才开始交出字节：record 要么整条交出，要么一个字节也不交
/// （Linux 同样要求读缓冲能容纳整条 record，否则 `EINVAL`）。
///
/// # Parameters
///
/// - `priority`: `facility << 3 | level`。
/// - `capacity`: 调用者缓冲的字节数。
/// - `emit`: 接收连续分片；返回错误时编码立即中止并原样传出。
///
/// # Errors
///
/// `emit` 返回的错误。
pub(super) fn encode<E>(
    priority: u8,
    sequence: u64,
    timestamp_us: u64,
    message: &[u8],
    capacity: usize,
    emit: &mut dyn FnMut(&[u8]) -> Result<(), E>,
) -> Result<Encoded, E> {
    let mut header = Header {
        bytes: [0; HEADER_CAPACITY],
        length: 0,
    };
    write!(header, "{priority},{sequence},{timestamp_us},-;")
        .expect("kmsg header exceeds its fixed capacity");
    let body: usize = message
        .iter()
        .map(|byte| if escapes(*byte) { 4 } else { 1 })
        .sum();
    if header.length + body + 1 > capacity {
        return Ok(Encoded::TooSmall);
    }
    emit(&header.bytes[..header.length])?;
    let mut chunk = [0u8; CHUNK];
    let mut filled = 0;
    for byte in message {
        if filled + 5 > CHUNK {
            emit(&chunk[..filled])?;
            filled = 0;
        }
        if escapes(*byte) {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            chunk[filled..filled + 4].copy_from_slice(&[
                b'\\',
                b'x',
                HEX[usize::from(byte >> 4)],
                HEX[usize::from(byte & 15)],
            ]);
            filled += 4;
        } else {
            chunk[filled] = *byte;
            filled += 1;
        }
    }
    // 循环保证追加前 `filled + 5 <= CHUNK`，所以结尾换行总有位置。
    chunk[filled] = b'\n';
    emit(&chunk[..=filled])?;
    Ok(Encoded::Done)
}

/// 解析 `/dev/kmsg` 写入的 printk 前缀（Linux `printk_parse_prefix` 的 `<N>` 形式）与结尾换行。
///
/// # Returns
///
/// `facility << 3 | level` 与消息体。没有前缀时 facility 为 `LOG_USER`、level 为
/// `default_level`；`<N>` 的 facility 为零同样取 `LOG_USER`。
pub(super) fn parse_user_message(input: &[u8], default_level: u8) -> (u8, &[u8]) {
    const LOG_USER: u8 = 1;
    let (value, mut text) = match input.strip_prefix(b"<") {
        Some(rest) => {
            let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
            match (digits, rest.get(digits)) {
                (1..=10, Some(b'>')) => {
                    let value = rest[..digits]
                        .iter()
                        .fold(0u64, |total, byte| total * 10 + u64::from(byte - b'0'));
                    (Some(value), &rest[digits + 1..])
                }
                _ => (None, input),
            }
        }
        None => (None, input),
    };
    if let Some(stripped) = text.strip_suffix(b"\n") {
        text = stripped;
    }
    let (facility, level) = match value {
        Some(value) => ((value >> 3) as u8 & 0x1f, (value & 7) as u8),
        None => (0, default_level),
    };
    let facility = if facility == 0 { LOG_USER } else { facility };
    (facility << 3 | level, text)
}
