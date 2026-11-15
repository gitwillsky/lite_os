//! 静态声明在同一确认选路图中的完整立即出口组。

use super::super::{HOP_BYTES, NextHop};
use super::{Candidate, valid_table};
use crate::{Decoder, Invalid, routing::Family, tracking};
use alloc::vec::Vec;

const VERSION: u32 = 1;
const HEADER: usize = 8;
/// 固定请求长度，含一张表、一基声明位置及双栈规范网段。
pub const REQUEST_BYTES: usize = 8 + Candidate::BYTES;

/// 需要观察的持久声明。外层管理回复将此位置限制在同一配置代际。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    /// 主表或备选表。
    pub table: u32,
    /// 持久声明位置和前缀。
    pub candidate: Candidate,
}

impl Request {
    fn valid(self) -> bool {
        valid_table(self.table) && self.candidate.prefix.valid()
    }

    fn family(self) -> Family {
        if self.candidate.prefix.network.is_ipv4() {
            Family::Ipv4
        } else {
            Family::Ipv6
        }
    }

    /// 编码只读精确请求。
    /// # Errors
    /// 表、前缀无效，或者内存不足。
    pub fn encode(self) -> Result<Vec<u8>, tracking::Error> {
        if !self.valid() {
            return Err(tracking::Error::Invalid);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(REQUEST_BYTES)
            .map_err(|_| tracking::Error::NoMemory)?;
        for value in [VERSION, self.table] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        self.candidate.write(&mut bytes);
        Ok(bytes)
    }

    /// 捕获一个规范声明选择器，拒绝尾部数据。
    /// # Errors
    /// 版本、长度、表、位置或前缀无效。
    pub fn parse(bytes: &[u8]) -> Result<Self, Invalid> {
        let mut input = Decoder::new(bytes);
        if input.u32()? != VERSION {
            return Err(Invalid);
        }
        let request = Self {
            table: input.u32()?,
            candidate: Candidate::read(&mut input)?,
        };
        input.finish()?;
        request.valid().then_some(request).ok_or(Invalid)
    }
}

/// 校验正文长度与完整成员数相符，空组仍有头。
pub(crate) fn valid_length(length: u64) -> bool {
    let header = HEADER as u64;
    let row = HOP_BYTES as u64;
    (header..=header + u64::from(u32::MAX) * row).contains(&length)
        && (length - header).is_multiple_of(row)
}

/// 按解析后的成员顺序编码整组。没有可解析出口时编码空组。
/// # Errors
/// 成员无效、计数超界、迭代数量不符或内存不足。
pub fn encode(
    request: Request,
    members: impl ExactSizeIterator<Item = Result<NextHop, Invalid>>,
) -> Result<Vec<u8>, tracking::Error> {
    let count = u32::try_from(members.len()).map_err(|_| tracking::Error::Invalid)?;
    let length = (count as usize)
        .checked_mul(HOP_BYTES)
        .and_then(|bytes| bytes.checked_add(HEADER))
        .ok_or(tracking::Error::Invalid)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| tracking::Error::NoMemory)?;
    output.extend_from_slice(&VERSION.to_le_bytes());
    output.extend_from_slice(&count.to_le_bytes());
    for (index, member) in members.enumerate() {
        let member = member.map_err(|_| tracking::Error::Invalid)?;
        if index >= count as usize || !member.valid(request.family()) {
            return Err(tracking::Error::Invalid);
        }
        member.write(&mut output);
    }
    if output.len() != length {
        return Err(tracking::Error::Invalid);
    }
    Ok(output)
}

/// 解码与请求族相符的完整的立即出口组。
/// # Errors
/// 版本、长度、成员无效或内存不足。
pub fn decode(request: Request, bytes: &[u8]) -> Result<Vec<NextHop>, tracking::Error> {
    let mut input = Decoder::new(bytes);
    let invalid = |_| tracking::Error::Invalid;
    if !valid_length(bytes.len() as u64) || input.u32().map_err(invalid)? != VERSION {
        return Err(tracking::Error::Invalid);
    }
    let count = input.u32().map_err(invalid)? as usize;
    if (bytes.len() - HEADER) / HOP_BYTES != count {
        return Err(tracking::Error::Invalid);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(count)
        .map_err(|_| tracking::Error::NoMemory)?;
    for _ in 0..count {
        let member = NextHop::read(&mut input)
            .map_err(invalid)?
            .ok_or(tracking::Error::Invalid)?;
        if !member.valid(request.family()) {
            return Err(tracking::Error::Invalid);
        }
        output.push(member);
    }
    Ok(output)
}
