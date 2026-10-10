//! 标准 proc_doulongvec_minmax/proc_dointvec numeric codec；实际限额与权限属于具体 namespace。
use super::super::device::{DeviceError, UserInput};
use crate::namespace::{NamespaceAuthority, UserNamespace, UserResource};
use alloc::{sync::Arc, vec::Vec};
use syscall_abi::{capability::Capability, errno};

/// 标准 base-0 unsigned integer、0..INT_MAX 与单值 consumed prefix；不接受符号或数字内垃圾。
fn whitespace(byte: &u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c')
}
fn decode_integer(
    bytes: &[u8],
    signed: bool,
    maximum: u64,
) -> Result<(u64, bool, usize), DeviceError> {
    let invalid = || DeviceError::Errno(errno::EINVAL);
    let mut position = bytes
        .iter()
        .position(|byte| !whitespace(byte))
        .ok_or_else(invalid)?;
    let start = position;
    let negative = signed && bytes[position] == b'-';
    if negative {
        position += 1;
    }
    if !bytes.get(position).is_some_and(u8::is_ascii_digit) {
        return Err(invalid());
    }
    let radix = if bytes[position] == b'0' {
        if bytes
            .get(position + 1)
            .is_some_and(|byte| matches!(byte, b'x' | b'X'))
            && bytes.get(position + 2).is_some_and(u8::is_ascii_hexdigit)
        {
            position += 2;
            16
        } else {
            8
        }
    } else {
        10
    };
    let mut value = 0u64;
    while let Some(&byte) = bytes.get(position) {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => break,
        };
        if digit >= radix {
            break;
        }
        value = value
            .checked_mul(u64::from(radix))
            .and_then(|value| value.checked_add(u64::from(digit)))
            .ok_or_else(invalid)?;
        position += 1;
    }
    if position - start >= 21
        || value > maximum.saturating_add(u64::from(negative))
        || bytes
            .get(position)
            .is_some_and(|byte| !matches!(byte, b' ' | b'\t' | b'\n'))
    {
        return Err(invalid());
    }
    while bytes.get(position).is_some_and(whitespace) {
        position += 1;
    }
    Ok((value, negative, position))
}
pub(super) fn write(
    target: &Arc<UserNamespace>,
    resource: UserResource,
    input: &mut dyn UserInput,
    offset: u64,
    caller: &NamespaceAuthority,
) -> Result<(), DeviceError> {
    if !caller.capable(target, Capability::SYS_RESOURCE) {
        return Err(DeviceError::Errno(errno::EPERM));
    }
    write_value(input, offset, 0, |limit| {
        target
            .set_resource_limit(resource, limit, caller)
            .map_err(|error| match error {
                crate::namespace::NamespaceError::Permission => DeviceError::Errno(errno::EPERM),
                _ => DeviceError::Errno(errno::EINVAL),
            })
    })
}
/// 两个标准单值sysctl共享base-0、page截取、user-copy、非零位置与consumed-prefix codec。
/// publish只在完整copy和范围检查成功后调用；实际limit/权限留在对应owner。
pub(super) fn write_value(
    input: &mut dyn UserInput,
    offset: u64,
    minimum: u32,
    publish: impl FnOnce(u32) -> Result<(), DeviceError>,
) -> Result<(), DeviceError> {
    write_scalar(
        input,
        offset,
        u64::from(minimum),
        i32::MAX as u64,
        |limit| publish(limit as u32),
    )
}
/// 标准 native unsigned long 的全范围 codec；不截断为 int，不接受负号。
pub(super) fn write_unsigned_long(
    input: &mut dyn UserInput,
    offset: u64,
    publish: impl FnOnce(u64) -> Result<(), DeviceError>,
) -> Result<(), DeviceError> {
    write_scalar(input, offset, 0, u64::MAX, publish)
}
fn write_scalar(
    input: &mut dyn UserInput,
    offset: u64,
    minimum: u64,
    maximum: u64,
    publish: impl FnOnce(u64) -> Result<(), DeviceError>,
) -> Result<(), DeviceError> {
    let Some(write) = NumericWrite::prepare(input, offset)? else {
        return Ok(());
    };
    let (limit, _, consumed) = decode_integer(&write.bytes[..write.copied], false, maximum)?;
    if limit < minimum {
        return Err(DeviceError::Errno(errno::EINVAL));
    }
    publish(limit)?;
    // 固定 proc_doulongvec_minmax 从原请求 count 减去 first-page 未消费部分。
    input.consume(write.count - write.copied + consumed);
    Ok(())
}

/// 实际 write buffer：所有 numeric handler 共用全量 fault 检查、first-page 与 strict offset 语义。
struct NumericWrite {
    bytes: Vec<u8>,
    count: usize,
    copied: usize,
}
impl NumericWrite {
    fn prepare(input: &mut dyn UserInput, offset: u64) -> Result<Option<Self>, DeviceError> {
        let count = input.remaining();
        if count == 0 {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(count)
            .map_err(|_| DeviceError::Errno(errno::ENOMEM))?;
        bytes.resize(count, 0);
        input
            .copy(&mut bytes)
            .map_err(|_| DeviceError::Errno(errno::EFAULT))?;
        if offset != 0 {
            input.consume(count);
            return Ok(None);
        }
        Ok(Some(Self {
            bytes,
            count,
            copied: count.min(4095),
        }))
    }
}
/// 标准四 int vector；解析失败仍发布已经接受的 prefix，之后才返回 EINVAL，不能伪造全事务 ABI。
pub(super) fn write_signed_vector(
    input: &mut dyn UserInput,
    offset: u64,
    publish: impl FnOnce(&[Option<i32>; 4]) -> Result<(), DeviceError>,
) -> Result<(), DeviceError> {
    let Some(write) = NumericWrite::prepare(input, offset)? else {
        return Ok(());
    };
    let bytes = &write.bytes[..write.copied];
    let mut values = [None; 4];
    let mut consumed = 0;
    let mut invalid = false;
    for value in &mut values {
        if bytes[consumed..].iter().all(whitespace) {
            consumed = bytes.len();
            break;
        }
        match decode_integer(&bytes[consumed..], true, i32::MAX as u64) {
            Ok((integer, negative, length)) => {
                *value = Some(if negative {
                    -(integer as i64)
                } else {
                    integer as i64
                } as i32);
                consumed += length;
            }
            Err(_) => {
                invalid = true;
                break;
            }
        }
    }
    if values.iter().all(Option::is_none) {
        return Err(DeviceError::Errno(errno::EINVAL));
    }
    publish(&values)?;
    if invalid {
        return Err(DeviceError::Errno(errno::EINVAL));
    }
    input.consume(write.count - write.copied + consumed);
    Ok(())
}
