//! 本地抓包控制面及 PCAP 2.4（小端纳秒、Ethernet）。

use crate::{Decoder, Invalid, bridge};
use alloc::vec::Vec;

const VERSION: u32 = 1;
/// 固定 PCAP 记录头长度。
pub const RECORD_BYTES: usize = 16;
/// 单帧捕获上限。完整记录连同 WebSocket 控制头可在同一消息中传输。
pub const SNAPLEN: usize = bridge::DATAGRAM_MAX - bridge::HEADER_BYTES - 1 - RECORD_BYTES;
/// PCAP 文件的最大字节数。
pub const FILE_LIMIT: u64 = 64 * 1024 * 1024;
/// 单次捕获的最大秒数。
pub const DURATION: u64 = 60;

/// PCAP 2.4 文件头，链路类型为 LINKTYPE_ETHERNET (1)。
pub const HEADER: [u8; 24] = {
    let length = (SNAPLEN as u32).to_le_bytes();
    [
        0x4d, 0x3c, 0xb2, 0xa1, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, length[0], length[1],
        length[2], length[3], 1, 0, 0, 0,
    ]
};

/// 一次正常结束的原因。
#[derive(Clone, Copy)]
#[repr(u8)]
pub enum End {
    Stopped,
    TimeLimit,
    SizeLimit,
    InterfaceDown,
}

/// 版本与非零 Linux 接口索引。
pub fn request(interface: u32) -> [u8; 8] {
    let mut bytes = [0; 8];
    bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
    bytes[4..].copy_from_slice(&interface.to_le_bytes());
    bytes
}

/// 读取本次抓取的接口索引。
/// # Errors
/// 格式或接口索引非法。
pub fn parse_request(bytes: &[u8]) -> Result<u32, Invalid> {
    let mut input = Decoder::new(bytes);
    if input.u32()? != VERSION {
        return Err(Invalid);
    }
    let index = input.u32()?;
    input.finish()?;
    if index == 0 || index > i32::MAX as u32 {
        return Err(Invalid);
    }
    Ok(index)
}

/// 抓包会话的文件头、完整记录批次或结束通知。
pub enum Frame<'a> {
    Header(&'a [u8]),
    Records { bytes: &'a [u8], packets: u32 },
    Finished { reason: End, drops: u32 },
}

impl<'a> Frame<'a> {
    /// 解码状态头后的正文，并验证记录边界。
    /// # Errors
    /// 状态、文件头、长度、时间戳或结束理由非法。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Invalid> {
        let (&tag, body) = bytes.split_first().ok_or(Invalid)?;
        match tag {
            1 if body == HEADER => Ok(Self::Header(body)),
            2 if !body.is_empty() => {
                let mut input = Decoder::new(body);
                let mut packets = 0;
                while !input.0.is_empty() {
                    input.u32()?;
                    let nanos = input.u32()?;
                    let length = input.u32()? as usize;
                    let original = input.u32()? as usize;
                    if nanos >= 1_000_000_000 || length < 14 || length != original.min(SNAPLEN) {
                        return Err(Invalid);
                    }
                    input.take(length)?;
                    packets += 1;
                }
                Ok(Self::Records {
                    bytes: body,
                    packets,
                })
            }
            3 => {
                let mut input = Decoder::new(body);
                let reason = match input.u8()? {
                    0 => End::Stopped,
                    1 => End::TimeLimit,
                    2 => End::SizeLimit,
                    3 => End::InterfaceDown,
                    _ => return Err(Invalid),
                };
                let drops = input.u32()?;
                input.finish()?;
                Ok(Self::Finished { reason, drops })
            }
            _ => Err(Invalid),
        }
    }
}

/// 写入一条 PCAP 记录，原始帧长可能大于保存的前缀。
/// # Errors
/// 帧长度或时间戳无法表示。
pub fn record(
    output: &mut Vec<u8>,
    seconds: u64,
    nanoseconds: u32,
    original: usize,
    bytes: &[u8],
) -> Result<(), Invalid> {
    if bytes.len() < 14 || bytes.len() != original.min(SNAPLEN) || nanoseconds >= 1_000_000_000 {
        return Err(Invalid);
    }
    let seconds = u32::try_from(seconds).map_err(|_| Invalid)?;
    let original = u32::try_from(original).map_err(|_| Invalid)?;
    for value in [seconds, nanoseconds, bytes.len() as u32, original] {
        output.extend_from_slice(&value.to_le_bytes());
    }
    output.extend_from_slice(bytes);
    Ok(())
}
