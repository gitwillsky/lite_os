#![no_std]
//! 无分配报文字节机制；借用原始 buffer，结构校验后读字段，原位构造头与校验和。
pub mod arp;
pub mod batch;
pub mod bgp;
pub mod buffer;
pub mod checksum;
pub mod dhcpv4;
pub mod dhcpv6;
pub mod dns;
pub mod dns_name;
pub mod ethernet;
pub mod icmp;
pub mod icmpv4;
pub mod icmpv6;
pub mod igmp;
pub mod ipv4;
pub mod ipv6;
pub mod mld;
pub mod multicast;
pub mod ndisc;
pub mod ospf;
pub mod spanning_tree;
pub mod tcp;
pub mod udp;

/// 报文结构或输出空间错误；不含 socket、设备与策略语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketError {
    /// 输入不足以容纳所声明的头或 payload。
    Truncated,
    /// 长度、字段组合或选项编码违反格式约束。
    Malformed,
    /// 必需的校验和不正确或缺失。
    Checksum,
    /// 当前格式版本或消息类型未定义。
    Unsupported,
    /// 输出 buffer 无法容纳完整报文。
    Capacity,
}
