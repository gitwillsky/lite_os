//! Linux timens_offsets proc codec；权限与冻结状态只由实际 TimeNamespace 拥有。
use super::{FileSystemError, ProcSource};
use crate::fs::chardev::{DeviceError, UserInput};
use crate::namespace::{NamespaceAuthority, TimeClock, TimeOffset, TimeOffsetError};
use alloc::vec::Vec;
use syscall_abi::errno;

pub(super) fn contents(source: &dyn ProcSource, tid: usize) -> Result<Vec<u8>, FileSystemError> {
    let target = source
        .thread_time_for_children(tid)
        .ok_or(FileSystemError::NotFound)?;
    let offsets = target.snapshot();
    super::super::try_format_bytes(format_args!(
        "monotonic {} {}\nboottime {} {}\n",
        offsets.monotonic.seconds,
        offsets.monotonic.nanoseconds,
        offsets.boottime.seconds,
        offsets.boottime.nanoseconds
    ))
}

fn invalid() -> DeviceError {
    DeviceError::Errno(errno::EINVAL)
}
fn whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c')
}
/// 固定 kernel vsscanf 的十进制 prefix：仅 signed 接受 '-'，不接受 '+'；LP64 运算按 u64 wrap。
fn integer(bytes: &[u8], position: &mut usize, signed: bool) -> Result<u64, DeviceError> {
    while bytes.get(*position).is_some_and(|byte| whitespace(*byte)) {
        *position += 1;
    }
    let negative = signed && bytes.get(*position) == Some(&b'-');
    if negative {
        *position += 1;
    }
    let start = *position;
    let mut number = 0u64;
    while let Some(byte) = bytes.get(*position).filter(|byte| byte.is_ascii_digit()) {
        number = number
            .wrapping_mul(10)
            .wrapping_add(u64::from(*byte - b'0'));
        *position += 1;
    }
    if start == *position {
        return Err(invalid());
    }
    Ok(if negative {
        number.wrapping_neg()
    } else {
        number
    })
}
fn line(bytes: &[u8]) -> Result<(TimeClock, TimeOffset), DeviceError> {
    let mut position = 0;
    while bytes.get(position).is_some_and(|byte| whitespace(*byte)) {
        position += 1;
    }
    let start = position;
    while bytes.get(position).is_some_and(|byte| !whitespace(*byte)) && position - start < 9 {
        position += 1;
    }
    let clock = match &bytes[start..position] {
        b"monotonic" | b"1" => TimeClock::Monotonic,
        b"boottime" | b"7" => TimeClock::Boottime,
        _ => return Err(invalid()),
    };
    let seconds = integer(bytes, &mut position, true)? as i64;
    let nanos = integer(bytes, &mut position, false)?;
    if nanos >= 1_000_000_000 {
        return Err(invalid());
    }
    Ok((
        clock,
        TimeOffset {
            seconds,
            nanoseconds: nanos as u32,
        },
    ))
}
pub(super) fn write(
    source: &dyn ProcSource,
    tid: usize,
    opener: &NamespaceAuthority,
    input: &mut dyn UserInput,
    offset: u64,
) -> Result<(), DeviceError> {
    let count = input.remaining();
    if offset != 0 || count >= 4096 {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(count)
        .map_err(|_| DeviceError::Errno(errno::ENOMEM))?;
    bytes.resize(count, 0);
    input
        .copy(&mut bytes)
        .map_err(|_| DeviceError::Errno(errno::EFAULT))?;
    let text = &bytes[..bytes.iter().position(|byte| *byte == 0).unwrap_or(count)];
    let mut changes = [(
        TimeClock::Monotonic,
        TimeOffset {
            seconds: 0,
            nanoseconds: 0,
        },
    ); 2];
    let mut used = 0;
    let mut position = 0;
    let mut consumed = count;
    loop {
        let end = text[position..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(text.len(), |end| position + end);
        changes[used] = line(&text[position..end])?;
        used += 1;
        position = end.saturating_add(1);
        if position >= text.len() {
            break;
        }
        if used == 2 {
            consumed = position;
            break;
        }
    }
    let target = source
        .thread_time_for_children(tid)
        .ok_or(DeviceError::Errno(errno::ESRCH))?;
    let now = crate::timer::get_time_ns();
    // 当前没有 suspend，host boottime 与 monotonic 同源；两个 namespace offset 独立。
    target
        .update(&changes[..used], (now, now), opener)
        .map_err(|error| {
            DeviceError::Errno(match error {
                TimeOffsetError::Invalid => errno::EINVAL,
                TimeOffsetError::Range => errno::ERANGE,
                TimeOffsetError::Permission => errno::EPERM,
                TimeOffsetError::Frozen => errno::EACCES,
            })
        })?;
    input.consume(consumed);
    Ok(())
}
