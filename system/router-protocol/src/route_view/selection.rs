//! 同一确认图中一组持久静态声明的选路状态。

use super::{read_address, write_address};
use crate::{Decoder, Encoder, Invalid, policy_routing, tracking};
use alloc::vec::Vec;
use core::num::{NonZeroU32, NonZeroU64};
use tracking::{Error, IpPrefix};

pub mod paths;

const VERSION: u32 = 3;
const REQUEST_HEADER: usize = 12;
const REPLY_HEADER: usize = 16;
const RESULT_BYTES: usize = 9;
/// 每个管理查询最多包含的声明数。
pub const LIMIT: usize = 64;
/// 最大有界回复，供原生通道分配控制帧。
pub const MAX_REPLY_BYTES: usize = REPLY_HEADER + LIMIT * RESULT_BYTES;

/// 配置表内的规范网段和一基声明位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// 声明的规范双栈网段。
    pub prefix: IpPrefix,
    /// 按持久配置代际解释的一基位置。
    pub location: NonZeroU32,
}

impl Candidate {
    const BYTES: usize = 22;

    fn write(self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&self.location.get().to_le_bytes());
        write_address(bytes, Some(self.prefix.network));
        bytes.push(self.prefix.length);
    }

    fn read(input: &mut Decoder<'_>) -> Result<Self, Invalid> {
        let location = NonZeroU32::new(input.u32()?).ok_or(Invalid)?;
        let prefix = IpPrefix {
            network: read_address(input)?.ok_or(Invalid)?,
            length: input.u8()?,
        };
        prefix
            .valid()
            .then_some(Self { prefix, location })
            .ok_or(Invalid)
    }
}

fn valid_table(table: u32) -> bool {
    table == policy_routing::TABLE_MAIN || policy_routing::alternate_table(table)
}

fn valid_length(length: u64, header: usize, width: usize) -> bool {
    let (header, width) = (header as u64, width as u64);
    (header + width..=header + LIMIT as u64 * width).contains(&length)
        && (length - header).is_multiple_of(width)
}

pub(crate) fn valid_request_length(length: u64) -> bool {
    valid_length(length, REQUEST_HEADER, Candidate::BYTES)
}

pub(crate) fn valid_reply_length(length: u64) -> bool {
    valid_length(length, REPLY_HEADER, RESULT_BYTES)
}

/// 一张表的有序候选查询。
pub struct Request {
    /// 主管理表或真实配置的备选表。
    pub table: u32,
    /// 与回复逐项对应的候选，允许跨两族。
    pub candidates: Vec<Candidate>,
}

impl Request {
    /// 编码指定表及候选。位置与前缀共同确定声明，位置不跨代际复用。
    /// # Errors
    /// 表、前缀或数量无效，或者内存不足。
    pub fn encode(table: u32, candidates: &[Candidate]) -> Result<Vec<u8>, Error> {
        if !valid_table(table)
            || !(1..=LIMIT).contains(&candidates.len())
            || !candidates.iter().all(|candidate| candidate.prefix.valid())
        {
            return Err(Error::Invalid);
        }
        let mut output = Encoder::default();
        output
            .0
            .try_reserve_exact(REQUEST_HEADER + candidates.len() * Candidate::BYTES)
            .map_err(|_| Error::NoMemory)?;
        output.u32(VERSION).u32(table).u32(candidates.len() as u32);
        for candidate in candidates {
            candidate.write(&mut output.0);
        }
        Ok(output.0)
    }

    /// 捕获一批有界选择器。
    /// # Errors
    /// 版本、表、条数、候选或尾部非法，或资源不足。
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if !valid_request_length(bytes.len() as u64) {
            return Err(Error::Invalid);
        }
        let mut input = Decoder::new(bytes);
        let field = |_| Error::Invalid;
        let version = input.u32().map_err(field)?;
        let table = input.u32().map_err(field)?;
        let count = input.u32().map_err(field)? as usize;
        if version != VERSION
            || !valid_table(table)
            || count != (bytes.len() - REQUEST_HEADER) / Candidate::BYTES
        {
            return Err(Error::Invalid);
        }
        let mut candidates = Vec::new();
        candidates
            .try_reserve_exact(count)
            .map_err(|_| Error::NoMemory)?;
        for _ in 0..count {
            candidates.push(Candidate::read(&mut input).map_err(field)?);
        }
        input.finish().map_err(field)?;
        Ok(Self { table, candidates })
    }
}

/// 策略处理后参与选路的度量。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metrics {
    /// 生效管理距离。
    pub distance: NonZeroU32,
    /// 该候选的路由代价。
    pub cost: u32,
}

/// 精确静态声明在当前确认图中的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// 未进入候选目录，例如停用、设备缺失，或被准入策略过滤。无生效度量。
    Absent,
    /// 已准入，下一跳不可用。
    Unreachable(Metrics),
    /// 可以解析，当前由其他候选转发。
    Standby(Metrics),
    /// 当前确认的入选项。
    Active(Metrics),
}

/// 一批只读结果，依次对应请求。
pub struct Reply {
    /// 同一个 RIB 实例的确认图代际，用来拒绝跨图拼接。
    pub generation: NonZeroU64,
    /// 每个候选的状态，保留请求顺序。
    pub rows: Vec<State>,
}

impl Reply {
    /// 编码某个确认图中的有序状态。
    /// # Errors
    /// 数量无效、迭代长度有误或分配失败。
    pub fn encode(
        generation: NonZeroU64,
        rows: impl ExactSizeIterator<Item = State>,
    ) -> Result<Vec<u8>, Error> {
        let count = rows.len();
        if !(1..=LIMIT).contains(&count) {
            return Err(Error::Invalid);
        }
        let length = REPLY_HEADER + count * RESULT_BYTES;
        let mut output = Encoder::default();
        output
            .0
            .try_reserve_exact(length)
            .map_err(|_| Error::NoMemory)?;
        output.u32(VERSION).u32(count as u32).u64(generation.get());
        for (index, row) in rows.enumerate() {
            if index >= count {
                return Err(Error::Invalid);
            }
            let (tag, metrics) = match row {
                State::Absent => (0, None),
                State::Unreachable(metrics) => (1, Some(metrics)),
                State::Standby(metrics) => (2, Some(metrics)),
                State::Active(metrics) => (3, Some(metrics)),
            };
            output
                .u8(tag)
                .u32(metrics.map_or(0, |item| item.distance.get()))
                .u32(metrics.map_or(0, |item| item.cost));
        }
        (output.0.len() == length)
            .then_some(output.0)
            .ok_or(Error::Invalid)
    }

    /// 捕获与请求逐项对应的结果。
    /// # Errors
    /// 版本、条数、代际、状态或长度无效，或者资源不足。
    pub fn parse(bytes: &[u8], expected: usize) -> Result<Self, Error> {
        let mut input = Decoder::new(bytes);
        let field = |_| Error::Invalid;
        if !(1..=LIMIT).contains(&expected)
            || bytes.len() != REPLY_HEADER + expected * RESULT_BYTES
            || input.u32().map_err(field)? != VERSION
            || input.u32().map_err(field)? as usize != expected
        {
            return Err(Error::Invalid);
        }
        let generation = NonZeroU64::new(input.u64().map_err(field)?).ok_or(Error::Invalid)?;
        let mut rows = Vec::new();
        rows.try_reserve_exact(expected)
            .map_err(|_| Error::NoMemory)?;
        for _ in 0..expected {
            let tag = input.u8().map_err(field)?;
            let distance = NonZeroU32::new(input.u32().map_err(field)?);
            let cost = input.u32().map_err(field)?;
            let metrics = distance.map(|distance| Metrics { distance, cost });
            rows.push(match (tag, metrics) {
                (0, None) if cost == 0 => State::Absent,
                (1, Some(metrics)) => State::Unreachable(metrics),
                (2, Some(metrics)) => State::Standby(metrics),
                (3, Some(metrics)) => State::Active(metrics),
                _ => return Err(Error::Invalid),
            });
        }
        input.finish().map_err(field)?;
        Ok(Self { generation, rows })
    }
}
