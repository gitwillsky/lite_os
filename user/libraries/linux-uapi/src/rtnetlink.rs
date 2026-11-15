//! 读取 Linux 路由 netlink 的实际接口、计数与地址。

use std::net::IpAddr;

#[cfg(target_os = "linux")]
mod reader;
#[cfg(target_os = "linux")]
pub use reader::LinkReader;

/// 一条标准接口记录的身份和收发统计。
#[derive(Clone, Copy)]
pub struct LinkStatistics {
    pub index: u32,
    /// Linux 接口名，尾部以零填充。
    pub name: [u8; 16],
    /// 六字节以太网链路地址。回环与其他链路类型没有这份身份。
    pub ethernet_address: Option<[u8; 6]>,
    /// IFF_UP，接口是否被管理员启用。
    pub up: bool,
    /// 收到、发出的报文数、字节数、错误数和丢弃数，各组接收在前。
    pub counters: [u64; 8],
}

/// 一条 RTM_GETADDR 记录。寿命以秒计，u32::MAX 表示无限期。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct LinkAddress {
    /// 本路由套接字所在网络命名空间内的 Linux ifindex。
    pub index: u32,
    /// 本机地址，点到点链路优先使用 IFA_LOCAL。
    pub address: IpAddr,
    pub prefix_length: u8,
    /// IFA_FLAGS，缺席时使用 ifaddrmsg 的低八位。
    pub flags: u32,
    /// 顺序为首选与有效寿命；内核未提供 IFA_CACHEINFO 时为 None。
    pub lifetimes: Option<(u32, u32)>,
}

impl LinkAddress {
    /// 地址是否还在进行重复地址检测，或检测到冲突。
    pub fn unavailable(self) -> bool {
        self.flags & (0x08 | 0x40) != 0
    }

    /// 内核是否把此地址标为永久。
    pub fn permanent(self) -> bool {
        self.flags & 0x80 != 0
    }
}
