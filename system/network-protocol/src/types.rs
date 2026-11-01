//! 网络服务与使用者共享的领域词汇：socket 错误与就绪状态、地址、选项、路由与接口快照。

use core::net::Ipv4Addr;

use device_model::network::NetworkStatistics;

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
}

const SOCKET_ERRORS: [SocketError; 30] = [
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
        SOCKET_ERRORS.get(usize::from(code).checked_sub(1)?).copied()
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

/// IPv4 socket 地址。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InetAddress {
    pub address: Ipv4Addr,
    pub port: u16,
}

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

/// sendmsg 单次 IPv4 源地址/出口选择；只随请求持有，不改变 endpoint binding。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InternetSendPacketInfo {
    pub interface_index: Option<u32>,
    pub source_address: Option<Ipv4Addr>,
}

/// 接收队列在实际 ingress 冻结的 IP_PKTINFO 事实；recvmsg 只编解码，不重新查路由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InternetPacketInfo {
    pub interface_index: u32,
    pub specific_address: Ipv4Addr,
    pub destination: Ipv4Addr,
}

/// 已由 socket request 验证的 IPv4 endpoint 类型；RAW 也拥有实际协议队列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InetProtocol {
    Udp,
    Tcp,
    Icmp,
    Raw,
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
}

const INTERNET_OPTIONS: [InternetOption; 15] = [
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
pub struct Ipv4RouteMetrics {
    pub advertised_mss: u32,
    pub receive_window: u32,
    pub round_trip_time: u32,
}

/// 路由类型的领域语义；数值由 syscall/netlink codec 拥有。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ipv4RouteKind {
    Unicast,
    Local,
    Broadcast,
    Unreachable,
    Throw,
}

const ROUTE_KINDS: [Ipv4RouteKind; 5] = [
    Ipv4RouteKind::Unicast,
    Ipv4RouteKind::Local,
    Ipv4RouteKind::Broadcast,
    Ipv4RouteKind::Unreachable,
    Ipv4RouteKind::Throw,
];

impl Ipv4RouteKind {
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

/// 已解码的标准路由配置；没有 raw sockaddr/用户指针。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4RouteRequest {
    pub destination: Ipv4Addr,
    pub prefix_length: u8,
    pub table: u32,
    pub metric: u32,
    pub scope: u8,
    pub protocol: u8,
    pub kind: Ipv4RouteKind,
    pub interface_index: Option<u32>,
    pub gateway: Option<Ipv4Addr>,
    pub preferred_source: Option<Ipv4Addr>,
    pub on_link: bool,
    pub metrics: Ipv4RouteMetrics,
}

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
}

const ROUTE_ERRORS: [Ipv4RouteError; 8] = [
    Ipv4RouteError::Invalid,
    Ipv4RouteError::NoDevice,
    Ipv4RouteError::NetworkDown,
    Ipv4RouteError::NetworkUnreachable,
    Ipv4RouteError::NoMemory,
    Ipv4RouteError::Exists,
    Ipv4RouteError::NotFound,
    Ipv4RouteError::Missing,
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

/// ifreq/rtnetlink 使用的实际接口快照；名称/index 不由 syscall 硬编码。
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
    /// 底层设备的驱动名；loopback 为 None。
    pub driver: Option<DriverName>,
}

impl InterfaceSnapshot {
    /// `SIOCGIFFLAGS` 与 rtnetlink `ifi_flags` 共用的 `IFF_*`，保证两条路径报告同一份状态。
    ///
    /// # Returns
    ///
    /// loopback 为 `IFF_LOOPBACK`，否则 `IFF_BROADCAST|IFF_MULTICAST`；链路 UP 时再加 `IFF_UP|IFF_RUNNING`。
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
        }) | if self.up { IFF_UP | IFF_RUNNING } else { 0 }
    }
}

/// 接口配置与计数器的当前快照；从实际域 owner 读取，不保存第二份配置。
#[derive(Clone, Copy, Debug)]
pub struct NetworkSnapshot {
    pub name: [u8; 16],
    pub statistics: NetworkStatistics,
}

/// 当前 FIB 记录及其设备名；快照没有可写路由权，也不派生缺省路由。
#[derive(Clone, Copy, Debug)]
pub struct RouteSnapshot {
    pub name: [u8; 16],
    pub route: Ipv4RouteRequest,
}
