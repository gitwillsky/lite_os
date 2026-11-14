//! 已授权的接口流量分析。每帧包含一个独立时间窗内的字节统计。

use alloc::vec::Vec;
use core::net::IpAddr;
use serde::{Deserialize, Serialize};

use crate::{Invalid, bridge};

pub use super::capture::{parse_request, request};

const VERSION: u8 = 1;
/// 单次最多展示的主要流量项。
pub const TOP_ROWS: usize = 12;

/// IP 地址与可见的端口，或 Ethernet 端点与协议。
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Endpoints {
    Ip {
        source: IpAddr,
        destination: IpAddr,
        protocol: u8,
        ports: Option<[u16; 2]>,
    },
    Ethernet {
        source: [u8; 6],
        destination: [u8; 6],
        ether_type: u16,
    },
}

/// 同一接口内按收发方向区分的流量项。
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub outgoing: bool,
    pub endpoints: Endpoints,
}

/// 可归属字节的下界与上界。没有替换误差时两者相等。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub key: Key,
    pub minimum_bytes: u64,
    pub maximum_bytes: u64,
}

/// 单个时间窗内已观察的流量、估计范围和采集丢包数。
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    pub interval_ns: u64,
    /// 依次为接收与发送的实际观察字节数，均包含可见链路头。
    pub bytes: [u64; 2],
    pub drops: u32,
    pub rows: Vec<Row>,
}

impl Sample {
    /// 编码可放入单条管理观察帧的聚合结果。
    /// # Errors
    /// 样本计数不一致、编码失败或超过控制帧容量。
    pub fn encode(&self) -> Result<Vec<u8>, Invalid> {
        self.validate()?;
        let mut bytes = serde_json::to_vec(self).map_err(|_| Invalid)?;
        bytes.insert(0, VERSION);
        if bytes.len() > bridge::DATAGRAM_MAX - bridge::HEADER_BYTES {
            return Err(Invalid);
        }
        Ok(bytes)
    }

    /// 解码并验证网关报告的一次独立采样。
    /// # Errors
    /// 版本、文档或计数不合法。
    pub fn parse(bytes: &[u8]) -> Result<Self, Invalid> {
        if bytes.len() > bridge::DATAGRAM_MAX - bridge::HEADER_BYTES {
            return Err(Invalid);
        }
        let body = bytes.strip_prefix(&[VERSION]).ok_or(Invalid)?;
        let result: Self = serde_json::from_slice(body).map_err(|_| Invalid)?;
        result.validate()?;
        Ok(result)
    }

    fn validate(&self) -> Result<(), Invalid> {
        if self.interval_ns == 0 || self.rows.len() > TOP_ROWS {
            return Err(Invalid);
        }
        self.bytes[0].checked_add(self.bytes[1]).ok_or(Invalid)?;
        let mut remaining = self.bytes;
        for (index, row) in self.rows.iter().enumerate() {
            let direction = usize::from(row.key.outgoing);
            if let Endpoints::Ip {
                source,
                destination,
                protocol,
                ports,
            } = row.key.endpoints
                && (source.is_ipv4() != destination.is_ipv4()
                    || ports.is_some() && !matches!(protocol, 6 | 17 | 132))
            {
                return Err(Invalid);
            }
            if row.minimum_bytes == 0
                || row.minimum_bytes > row.maximum_bytes
                || row.maximum_bytes > self.bytes[direction]
                || self.rows[..index].iter().any(|other| other.key == row.key)
            {
                return Err(Invalid);
            }
            remaining[direction] = remaining[direction]
                .checked_sub(row.minimum_bytes)
                .ok_or(Invalid)?;
        }
        Ok(())
    }
}
