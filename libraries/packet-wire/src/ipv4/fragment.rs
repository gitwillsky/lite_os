//! RFC 791 分片/再分片的无状态窗口；实际提交游标、策略和载荷归调用者。
use super::{checksum, validate_options};
use crate::PacketError;
use core::ops::Range;
/// 在 output 中填写下一片 IPv4 header，返回原载荷中的借用范围；不复制 payload。
/// # Parameters
/// `original` 是含完整 options 的原报头，`payload` 为其后载荷长度；`offset` 是已真实提交的字节数。
/// `mtu` 是当前实际发送上限，`output` 至少与原 header 等长；允许调用者先修改 TTL/选项，checksum 会重算。
/// # Returns
/// 本片相对原载荷的范围；范围结尾等于 payload 时本次原包发送完成，原 MF/offset 仍保留再分片语义。
/// # Errors
/// 头/游标/范围格式错误、DF 禁止分片或 MTU/输出空间不足；失败不写 output。
pub fn header(
    original: &[u8],
    payload: usize,
    offset: usize,
    mtu: usize,
    output: &mut [u8],
) -> Result<Range<usize>, PacketError> {
    let size = original.len();
    if !(20..=60).contains(&size)
        || !size.is_multiple_of(4)
        || original[0] >> 4 != 4
        || usize::from(original[0] & 15) * 4 != size
        || size.checked_add(payload)
            != Some(usize::from(u16::from_be_bytes([original[2], original[3]])))
        || offset > payload
        || offset != 0 && offset == payload
        || !offset.is_multiple_of(8)
    {
        return Err(PacketError::Malformed);
    }
    validate_options(&original[20..])?;
    if output.len() < size || mtu < size {
        return Err(PacketError::Capacity);
    }
    let flags = u16::from_be_bytes([original[6], original[7]]);
    if flags & 0x8000 != 0
        || flags & 0x4000 != 0 && flags & 0x3fff != 0
        || flags & 0x3fff != 0 && payload == 0
    {
        return Err(PacketError::Malformed);
    }
    if flags & 0x4000 != 0 && size + payload > mtu {
        return Err(PacketError::Capacity);
    }
    let remaining = payload - offset;
    let length = if remaining <= mtu - size {
        remaining
    } else {
        (mtu - size) & !7
    };
    if remaining != 0 && length == 0 {
        return Err(PacketError::Capacity);
    }
    let absolute = usize::from(flags & 0x1fff) * 8 + offset;
    let more = length != remaining || flags & 0x2000 != 0;
    if absolute + length > 65515 || more && !length.is_multiple_of(8) {
        return Err(PacketError::Malformed);
    }
    let output = &mut output[..size];
    output.copy_from_slice(original);
    if absolute != 0 {
        let mut at = 20;
        while at < size {
            match output[at] {
                0 => break,
                1 => at += 1,
                kind => {
                    let length = usize::from(output[at + 1]);
                    if kind & 0x80 == 0 {
                        output[at..at + length].fill(1);
                    }
                    at += length;
                }
            }
        }
    }
    output[2..4].copy_from_slice(&((size + length) as u16).to_be_bytes());
    output[6..8].copy_from_slice(
        &((absolute / 8) as u16 | if more { 0x2000 } else { 0 } | flags & 0x4000).to_be_bytes(),
    );
    output[10..12].fill(0);
    let value = checksum::checksum(output);
    output[10..12].copy_from_slice(&value.to_be_bytes());
    Ok(offset..offset + length)
}
