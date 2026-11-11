//! 网络服务与使用者共享的领域词汇：socket 错误与就绪状态、地址、选项、路由与接口快照。

pub use crate::configuration::ConfigurationBatch;
pub use crate::namespace::addresses::{
    AddressOrigin, AddressSnapshot, AddressState, InterfaceAddress,
};
pub use crate::namespace::neighbors::{Neighbor, NeighborSnapshot, NeighborState};
pub use crate::routes::snapshot::{RouteSnapshot, RoutingSnapshot};
use alloc::vec::Vec;
use core::net::{Ipv4Addr, Ipv6Addr};

use device_model::network::{NetworkLink, NetworkStatistics};

/// socket 操作的失败；使用者按自己的 ABI 映射为错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketError {
    Invalid,
    NoMemory,
    /// 实际接收队列过载；netlink错误一次消费后保留已排队datagram。
    NoBuffer,
    /// AF_UNIX autobind 已搜索全部 2^20 名称且无空位。
    NoSpace,
    AddressInUse,
    AddressNotAvailable,
    NotConnected,
    AlreadyConnected,
    /// active connect 已启动，调用方应进入 writable/error wait。
    InProgress,
    /// 同一 endpoint 已有尚未完成的 active connect。
    AlreadyInProgress,
    ConnectionRefused,
    /// established stream 被 peer reset。
    ConnectionReset,
    ConnectionAborted,
    TimedOut,
    /// Ethernet adapter 在异步协议推进期间报告设备 I/O failure。
    Device,
    NetworkUnreachable,
    HostUnreachable,
    DestinationRequired,
    MessageTooLarge,
    ProtocolNotSupported,
    OperationNotSupported,
    Again,
    BrokenPipe,
    PermissionDenied,
    OperationNotPermitted,
    NoDevice,
    /// packet send查不到实际出口（ENXIO），与bind的ENODEV不同。
    DeviceNotAvailable,
    NetworkDown,
    WrongType,
    /// real-UID SCM_RIGHTS inflight 超过 sender RLIMIT_NOFILE。
    TooManyReferences,
    /// 请求的 IP 版本不属于该 socket；不能截断或隐式转成另一族。
    AddressFamilyNotSupported,
    /// 精确密钥或其他 socket 配置对象不存在（ENOENT）。
    NoEntry,
    ProtocolError,
    ProtocolOptionUnavailable,
    HostDown,
    NetworkIsolated,
}

const SOCKET_ERRORS: [SocketError; 36] = [
    SocketError::Invalid,
    SocketError::NoMemory,
    SocketError::NoBuffer,
    SocketError::NoSpace,
    SocketError::AddressInUse,
    SocketError::AddressNotAvailable,
    SocketError::NotConnected,
    SocketError::AlreadyConnected,
    SocketError::InProgress,
    SocketError::AlreadyInProgress,
    SocketError::ConnectionRefused,
    SocketError::ConnectionReset,
    SocketError::ConnectionAborted,
    SocketError::TimedOut,
    SocketError::Device,
    SocketError::NetworkUnreachable,
    SocketError::HostUnreachable,
    SocketError::DestinationRequired,
    SocketError::MessageTooLarge,
    SocketError::ProtocolNotSupported,
    SocketError::OperationNotSupported,
    SocketError::Again,
    SocketError::BrokenPipe,
    SocketError::PermissionDenied,
    SocketError::OperationNotPermitted,
    SocketError::NoDevice,
    SocketError::DeviceNotAvailable,
    SocketError::NetworkDown,
    SocketError::WrongType,
    SocketError::TooManyReferences,
    SocketError::AddressFamilyNotSupported,
    SocketError::NoEntry,
    SocketError::ProtocolError,
    SocketError::ProtocolOptionUnavailable,
    SocketError::HostDown,
    SocketError::NetworkIsolated,
];

impl SocketError {
    /// 线编码（非零）。
    pub fn code(self) -> u8 {
        SOCKET_ERRORS
            .iter()
            .position(|error| *error == self)
            .map_or(0, |index| index as u8 + 1)
    }

    /// 线解码；未知编码为 `None`。
    pub fn from_code(code: u8) -> Option<Self> {
        SOCKET_ERRORS
            .get(usize::from(code).checked_sub(1)?)
            .copied()
    }
}

/// socket 的电平就绪状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketPollState {
    pub readable: bool,
    pub writable: bool,
    pub hangup: bool,
    pub read_closed: bool,
    pub error: bool,
}

impl SocketPollState {
    /// 已退休 backend observer 的终态；不再访问可能复用的协议 slot。
    pub const fn closed() -> Self {
        Self {
            readable: true,
            writable: false,
            hangup: true,
            read_closed: true,
            error: false,
        }
    }

    pub const fn error() -> Self {
        Self {
            readable: false,
            writable: false,
            hangup: false,
            read_closed: false,
            error: true,
        }
    }
}

/// 原生 Internet endpoint 使用标准双族值；IPv6 同时保留 flowinfo 与接口 zone，不借用 Linux sockaddr 布局。
pub use core::net::SocketAddr as InetAddress;

/// `sockaddr_ll` 的领域值，不含 userspace padding。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketAddress {
    pub protocol: u16,
    pub interface_index: i32,
    pub hardware_type: u16,
    pub packet_type: u8,
    pub address_length: u8,
    pub address: [u8; 8],
}

/// sendmsg 的一个 packet-info 控制项；按原顺序传递，由实际目的族的协议 owner 验证。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternetSendPacketInfo {
    Ipv4 {
        interface_index: Option<u32>,
        source_address: Ipv4Addr,
    },
    Ipv6 {
        interface_index: Option<u32>,
        source_address: Ipv6Addr,
    },
    /// 已识别的控制项宽度错误；目的族尚未确定，不能提前拒绝会被该族忽略的项。
    Invalid(InetFamily),
}

/// 实际接收时按当前请求选出的 packet info；IPv4 双栈消息可要求两种标准投影，不复制目的地址。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternetPacketInfo {
    Ipv4 {
        interface_index: u32,
        specific_address: Ipv4Addr,
        destination: Ipv4Addr,
    },
    Ipv6 {
        interface_index: u32,
        destination: Ipv6Addr,
    },
    DualStack {
        interface_index: u32,
        specific_address: Ipv4Addr,
        destination: Ipv4Addr,
    },
}
impl InternetPacketInfo {
    /// 返回要求交付的 IPv4 接口/选源地址/实际目的；其他投影返回 None。
    pub fn ipv4(self) -> Option<(u32, Ipv4Addr, Ipv4Addr)> {
        match self {
            Self::Ipv4 {
                interface_index,
                specific_address,
                destination,
            }
            | Self::DualStack {
                interface_index,
                specific_address,
                destination,
            } => Some((interface_index, specific_address, destination)),
            Self::Ipv6 { .. } => None,
        }
    }
    /// 返回要求交付的 IPv6 接口/实际目的；双栈 IPv4 目的使用 mapped 地址，其他投影返回 None。
    pub fn ipv6(self) -> Option<(u32, Ipv6Addr)> {
        match self {
            Self::Ipv6 {
                interface_index,
                destination,
            } => Some((interface_index, destination)),
            Self::DualStack {
                interface_index,
                destination,
                ..
            } => Some((interface_index, destination.to_ipv6_mapped())),
            Self::Ipv4 { .. } => None,
        }
    }
}

/// 创建时选择的 Internet 地址族；与 UDP/TCP 等传输协议分开，不含 Linux ABI 编号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InetFamily {
    Ipv4,
    Ipv6,
}

/// 已由 socket request 验证的传输类型；RAW 也拥有实际协议队列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InetProtocol {
    Udp,
    Tcp,
    /// An exact nonzero IP protocol number; 255 retains IPPROTO_RAW send-only semantics.
    Raw(u8),
}

/// packet socket 的链路头处理方式：`Raw` 收发含以太网头的帧，`Datagram` 只收发负载。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketKind {
    Raw,
    Datagram,
}

/// 已实现的 INET 选项；实际值只由协议 owner 保存，ABI 编号留在使用者的 syscall 层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternetOption {
    ReuseAddress,
    Broadcast,
    BoundToDevice,
    HopLimit,
    HeaderIncluded,
    NoDelay,
    MaxSegment,
    PacketInfo,
    KeepAlive,
    KeepIdle,
    KeepInterval,
    KeepCount,
    SynRetries,
    UserTimeout,
    Linger2,
    Ipv6Only,
    Ipv6HopLimit,
    Ipv6PacketInfo,
    /// 已关联 IPv6 socket 当前路径 MTU，不是显式发送上限的回读。
    Ipv6Mtu,
    /// IPv6 RAW 校验和偏移；负数表示禁用。
    Ipv6Checksum,
    /// 实际 IP 协议号，不是传输类型的线编码。
    Protocol,
    /// 接收本接口上其他 socket 加入的 IPv6 组播。
    Ipv6MulticastAll,
    /// IPv6 组播默认发送接口；0 表示由网络域选择。
    Ipv6MulticastInterface,
    /// IPv6 组播跳数，独立于单播跳数。
    Ipv6MulticastHopLimit,
    /// IPv6 组播是否回送到本机成员。
    Ipv6MulticastLoop,
    /// 接收接口上其他 socket 加入的 IPv4 组播。
    Ipv4MulticastAll,
    /// IPv4 组播发送的地址选择意图，按逻辑 u32 返回；不是接口序号。
    Ipv4MulticastInterface,
    /// IPv4 组播 TTL，独立于单播 TTL。
    Ipv4MulticastHopLimit,
    /// IPv4 组播是否回送到本机成员。
    Ipv4MulticastLoop,
    /// IPv4 路径 MTU 发现模式，逻辑值 0..5。
    Ipv4MtuDiscovery,
    Ipv6DontFragment,
    Ipv4Mtu,
    Ipv4ReceiveError,
    Ipv6ReceiveError,
    /// 本机发送使用的 32 位路由标记。
    Mark,
}

const INTERNET_OPTIONS: [InternetOption; 35] = [
    InternetOption::ReuseAddress,
    InternetOption::Broadcast,
    InternetOption::BoundToDevice,
    InternetOption::HopLimit,
    InternetOption::HeaderIncluded,
    InternetOption::NoDelay,
    InternetOption::MaxSegment,
    InternetOption::PacketInfo,
    InternetOption::KeepAlive,
    InternetOption::KeepIdle,
    InternetOption::KeepInterval,
    InternetOption::KeepCount,
    InternetOption::SynRetries,
    InternetOption::UserTimeout,
    InternetOption::Linger2,
    InternetOption::Ipv6Only,
    InternetOption::Ipv6HopLimit,
    InternetOption::Ipv6PacketInfo,
    InternetOption::Ipv6Mtu,
    InternetOption::Ipv6Checksum,
    InternetOption::Protocol,
    InternetOption::Ipv6MulticastAll,
    InternetOption::Ipv6MulticastInterface,
    InternetOption::Ipv6MulticastHopLimit,
    InternetOption::Ipv6MulticastLoop,
    InternetOption::Ipv4MulticastAll,
    InternetOption::Ipv4MulticastInterface,
    InternetOption::Ipv4MulticastHopLimit,
    InternetOption::Ipv4MulticastLoop,
    InternetOption::Ipv4MtuDiscovery,
    InternetOption::Ipv6DontFragment,
    InternetOption::Ipv4Mtu,
    InternetOption::Ipv4ReceiveError,
    InternetOption::Ipv6ReceiveError,
    InternetOption::Mark,
];

impl InternetOption {
    pub(crate) fn code(self) -> u8 {
        INTERNET_OPTIONS
            .iter()
            .position(|option| *option == self)
            .map_or(0, |index| index as u8)
    }

    pub(crate) fn from_code(code: u8) -> Option<Self> {
        INTERNET_OPTIONS.get(usize::from(code)).copied()
    }
}

/// 路由附带的传输参数（`RTAX_ADVMSS`/`RTAX_WINDOW`/`RTAX_RTT`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RouteMetrics {
    pub advertised_mss: u32,
    pub receive_window: u32,
    pub round_trip_time: u32,
}

/// 路由类型的领域语义；数值由 syscall/netlink codec 拥有。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteKind {
    Unicast,
    Local,
    Broadcast,
    Unreachable,
    Throw,
    /// 静默丢弃；本机发送按 ABI 映射为 Invalid。
    Blackhole,
    /// 管理策略禁止；本机发送按 ABI 映射为 PermissionDenied。
    Prohibit,
}

const ROUTE_KINDS: [RouteKind; 7] = [
    RouteKind::Unicast,
    RouteKind::Local,
    RouteKind::Broadcast,
    RouteKind::Unreachable,
    RouteKind::Throw,
    RouteKind::Blackhole,
    RouteKind::Prohibit,
];

impl RouteKind {
    pub(crate) fn code(self) -> u8 {
        ROUTE_KINDS
            .iter()
            .position(|kind| *kind == self)
            .map_or(0, |index| index as u8)
    }

    pub(crate) fn from_code(code: u8) -> Option<Self> {
        ROUTE_KINDS.get(usize::from(code)).copied()
    }
}

/// 已解码的类型化路由配置；没有 raw sockaddr/用户指针。
/// A 表示目标和首选源的地址族；IPv4 允许使用 IPv6 网关。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteRequest<A: crate::routes::RouteAddress> {
    pub destination: A,
    pub prefix_length: u8,
    pub table: u32,
    pub metric: u32,
    pub scope: u8,
    pub protocol: u8,
    pub kind: RouteKind,
    pub interface_index: Option<u32>,
    pub gateway: Option<A::Gateway>,
    pub preferred_source: Option<A>,
    pub on_link: bool,
    pub metrics: RouteMetrics,
}

/// AF_INET 配置边界使用的具体请求；与双栈执行批次共用同一字段模型，没有另一份 codec。
pub type Ipv4RouteRequest = RouteRequest<Ipv4Addr>;

/// 路由配置的失败（FIB 准入），不借 socket 的传输生命周期表达 ESRCH/EEXIST。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ipv4RouteError {
    Invalid,
    NoDevice,
    NetworkDown,
    NetworkUnreachable,
    NoMemory,
    Exists,
    NotFound,
    Missing,
    /// 原生配置事务独占写入，调用者可稍后重试。
    Again,
}

const ROUTE_ERRORS: [Ipv4RouteError; 9] = [
    Ipv4RouteError::Invalid,
    Ipv4RouteError::NoDevice,
    Ipv4RouteError::NetworkDown,
    Ipv4RouteError::NetworkUnreachable,
    Ipv4RouteError::NoMemory,
    Ipv4RouteError::Exists,
    Ipv4RouteError::NotFound,
    Ipv4RouteError::Missing,
    Ipv4RouteError::Again,
];

impl Ipv4RouteError {
    /// 对应的 Linux errno（正值）；ioctl 与 rtnetlink 共用，保证两条配置路径报告同一错误。
    pub const fn errno(self) -> i32 {
        match self {
            Self::Invalid => 22,             // EINVAL
            Self::NoDevice => 19,            // ENODEV
            Self::NetworkDown => 100,        // ENETDOWN
            Self::NetworkUnreachable => 101, // ENETUNREACH
            Self::NoMemory => 12,            // ENOMEM
            Self::Exists => 17,              // EEXIST
            Self::NotFound => 3,             // ESRCH
            Self::Missing => 2,              // ENOENT
            Self::Again => 11,               // EAGAIN
        }
    }

    pub(crate) fn code(self) -> u8 {
        ROUTE_ERRORS
            .iter()
            .position(|error| *error == self)
            .map_or(0, |index| index as u8 + 1)
    }

    pub(crate) fn from_code(code: u8) -> Option<Self> {
        ROUTE_ERRORS.get(usize::from(code).checked_sub(1)?).copied()
    }
}

/// 路由配置请求的 netlink 语义标志（`NLM_F_REPLACE`/`CREATE`/`EXCL`/`APPEND` 与删除）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RouteChange {
    pub delete: bool,
    pub replace: bool,
    pub create: bool,
    pub exclusive: bool,
    pub append: bool,
}

/// 驱动名的定长存储；网络服务把设备驱动名交给使用者的 sysfs/procfs 投影。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriverName {
    bytes: [u8; 16],
    length: u8,
}

impl DriverName {
    /// 截断到 16 字节。
    pub fn new(name: &str) -> Self {
        let mut bytes = [0u8; 16];
        let length = name.len().min(bytes.len());
        bytes[..length].copy_from_slice(&name.as_bytes()[..length]);
        Self {
            bytes,
            length: length as u8,
        }
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..usize::from(self.length)]).unwrap_or("")
    }
}

/// 实际接口的轻量事实；完整地址由 AddressSnapshot 提供，不为收发路径引入动态列表。
#[derive(Clone, Copy, Debug)]
pub struct InterfaceSnapshot {
    pub index: u32,
    pub name: [u8; 16],
    pub loopback: bool,
    pub mtu: i32,
    pub mac: [u8; 6],
    pub address: Option<Ipv4Addr>,
    pub prefix_length: u8,
    pub broadcast: Ipv4Addr,
    pub up: bool,
    /// 物理链路事实；回环或驱动读取失败时为 None。
    pub link: Option<NetworkLink>,
    /// 底层设备的驱动名；loopback 为 None。
    pub driver: Option<DriverName>,
}

impl InterfaceSnapshot {
    /// 返回已知载波；无物理设备的回环按始终有载波处理。
    pub fn carrier(&self) -> Option<bool> {
        if self.loopback {
            Some(true)
        } else {
            self.link.map(|link| link.carrier)
        }
    }

    /// Linux IF_OPER_UNKNOWN、IF_OPER_DOWN 或 IF_OPER_UP。
    pub fn operstate(&self) -> u8 {
        match (self.up, self.loopback, self.carrier()) {
            (false, _, _) | (_, _, Some(false)) => 2,
            (true, true, _) | (_, _, None) => 0,
            (true, false, Some(true)) => 6,
        }
    }

    /// `SIOCGIFFLAGS` 与 rtnetlink `ifi_flags` 共用的 `IFF_*`，保证两条路径报告同一份状态。
    ///
    /// # Returns
    ///
    /// 管理启用与实际可运行分别映射为 `IFF_UP` 和 `IFF_RUNNING`。
    pub fn flags(&self) -> u16 {
        const IFF_UP: u16 = 0x1;
        const IFF_BROADCAST: u16 = 0x2;
        const IFF_LOOPBACK: u16 = 0x8;
        const IFF_RUNNING: u16 = 0x40;
        const IFF_MULTICAST: u16 = 0x1000;
        (if self.loopback {
            IFF_LOOPBACK
        } else {
            IFF_BROADCAST | IFF_MULTICAST
        }) | if self.up {
            IFF_UP
                | if self.carrier() == Some(true) {
                    IFF_RUNNING
                } else {
                    0
                }
        } else {
            0
        }
    }
}

/// 接口配置与计数器的当前快照；从实际域 owner 读取，不保存第二份配置。
#[derive(Clone, Copy, Debug)]
pub struct NetworkSnapshot {
    pub name: [u8; 16],
    pub statistics: NetworkStatistics,
}

/// 捕获时的 transport checksum 状态；Partial 与 Verified 互斥。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketChecksum {
    /// 未提供硬件验证信息，接收者自行检查。
    Unverified,
    /// transport checksum 尚待设备补全，字节中仍包含伪头 seed。
    Partial,
    /// 已由设备验证，或驱动在交付前补全。
    Verified,
}

/// 随实际捕获帧冻结的校验和状态与网络头位置；不包含 Linux cmsg 编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketInformation {
    pub checksum: PacketChecksum,
    /// 相对本次 socket 交付内容的 L3 header 偏移。
    pub network_offset: u16,
}

/// 一次 INET receive 的结果。
pub struct InetReceived {
    pub data: Vec<u8>,
    /// 消息的完整长度（datagram 截断时大于 `data.len()`）。
    pub full_length: usize,
    pub source: Option<InetAddress>,
    pub packet_info: Option<InternetPacketInfo>,
}

/// 由发送端点收到的错误来源。Linux errno 和 cmsg 布局由兼容层编码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InternetErrorOrigin {
    Local,
    Icmpv4,
    Icmpv6,
}

/// 一条独立的发送错误及原目的。offender 无端口，局部错误没有 offender。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InternetError {
    pub error: SocketError,
    pub origin: InternetErrorOrigin,
    pub kind: u8,
    pub code: u8,
    pub info: u32,
    pub destination: InetAddress,
    pub offender: Option<InetAddress>,
}

/// 已从错误队列消费的记录。data 只包含实际可见的引用部分。
pub struct InetErrorReceived {
    pub data: Vec<u8>,
    pub full_length: usize,
    pub error: InternetError,
}

/// 一次 packet receive 的结果。
pub struct PacketReceived {
    pub data: Vec<u8>,
    pub full_length: usize,
    pub source: PacketAddress,
    pub information: PacketInformation,
}
