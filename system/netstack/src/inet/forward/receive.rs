//! 收包线程只检查原始字节。源、目的准入及 ICMP 回复仍由实际协议域决定。

use super::{SendResult, ipv6};
use device_model::network::{NetworkReceiveFrame, ReceivePacket};
use packet_wire::ipv4::Ipv4Packet;

const CHECK_MASK: u64 = 7;
const VALID: u64 = 1;
const DISCARD: u64 = 2;
const BAD_FIELD: u64 = 3;
const BAD_OPTION: u64 = 4;
const FRAGMENT_CHECKED: u64 = 1 << 3;
const FRAGMENT_PRESENT: u64 = 1 << 4;
const POINTER_SHIFT: u32 = 32;

/// 与一个原始物理帧绑定的网络头检查；零表示尚未检查。
pub(in crate::inet) struct ReceiveHeader(u64);

impl ReceiveHeader {
    /// 生成首部校验、逐跳裁决和可选分片定位事实，禁止在队列线程产生协议副作用。
    pub(in crate::inet) fn prepare(packet: ReceivePacket<'_>, inspect_fragments: bool) -> u64 {
        match packet {
            ReceivePacket::Ipv4(ip) => {
                if ip.check_header_checksum().is_ok() {
                    VALID
                } else {
                    DISCARD
                }
            }
            ReceivePacket::Ipv6(ip) => {
                let hop = if ip.bytes()[6] == 0 {
                    match ipv6::hop_options(ip) {
                        Ok(()) => VALID,
                        Err(SendResult::Discard) => DISCARD,
                        Err(SendResult::Error(4, code @ (0 | 2), pointer)) => {
                            let kind = if code == 0 { BAD_FIELD } else { BAD_OPTION };
                            (u64::from(pointer) << POINTER_SHIFT) | kind
                        }
                        Err(_) => unreachable!("Hop-by-Hop only rejects or reports a parameter problem"),
                    }
                } else {
                    0
                };
                if inspect_fragments {
                    let fragment = if ip.has_fragment_header() { FRAGMENT_PRESENT } else { 0 };
                    hop | FRAGMENT_CHECKED | fragment
                } else {
                    hop
                }
            }
            _ => 0,
        }
    }

    /// 读取原物理帧的事实。逻辑入口和重组包没有此提示。
    pub(in crate::inet) fn from_frame(frame: Option<&NetworkReceiveFrame<'_>>) -> Self {
        use crate::inet::protocol_owner::RECEIVE_FORWARD_HEADER;
        Self(frame.map_or(0, |frame| frame.annotation()[RECEIVE_FORWARD_HEADER]))
    }

    /// 核对同一 IPv4 首部的完整校验和。仅在队列尚未检查时读取报文字节。
    pub(in crate::inet) fn ipv4_valid(&self, ip: Ipv4Packet<'_>) -> bool {
        match self.0 & CHECK_MASK {
            VALID => true,
            DISCARD => false,
            _ => ip.check_header_checksum().is_ok(),
        }
    }

    pub(super) fn ipv6_hop(&self) -> Option<Result<(), SendResult>> {
        let pointer = (self.0 >> POINTER_SHIFT) as u32;
        match self.0 & CHECK_MASK {
            VALID => Some(Ok(())),
            DISCARD => Some(Err(SendResult::Discard)),
            BAD_FIELD => Some(Err(SendResult::Error(4, 0, pointer))),
            BAD_OPTION => Some(Err(SendResult::Error(4, 2, pointer))),
            _ => None,
        }
    }

    pub(super) fn ipv6_fragment(&self) -> Option<bool> {
        (self.0 & FRAGMENT_CHECKED != 0).then_some(self.0 & FRAGMENT_PRESENT != 0)
    }
}
