#![no_std]

//! 根服务与各系统服务之间的启动约定：bootfs 中的映像路径、链接基址与角色句柄下标。
//!
//! 角色句柄下标是启动消息（`native_abi::startup`）中的绝对下标，从 `ROLE_HANDLES` 开始。

use native_abi::startup::ROLE_HANDLES;

/// Linux 兼容层服务。
///
/// 运行形态（Starnix 模型）：映像装载在共享进程的共享区，每个 Linux 进程是该共享进程的一个成员；
/// 成员共用句柄表，服务线程在任一成员中运行时看到同一份服务状态。
pub mod linuxd {
    use super::ROLE_HANDLES;

    /// bootfs 中的映像路径。
    pub const IMAGE: &[u8] = b"bin/linuxd";
    /// 静态链接基址；必须位于两种架构的共享区内（`SHARED_START` 不超过该值）。
    pub const BASE: usize = 0x20_0000_0000;
    /// 共享进程（创建 Linux 进程所需的 `process_create_shared` 原型）。
    pub const SHARED_PROCESS: usize = ROLE_HANDLES;
    /// 容纳全部 Linux 进程的 Job；服务 panic 时整体终止。
    pub const JOB: usize = ROLE_HANDLES + 1;
    /// 根 resource（串口、debuglog、时钟与电源）。
    pub const RESOURCE: usize = ROLE_HANDLES + 2;
    /// 根服务路由给 linuxd 的设备目录连接（`device_protocol::directory`，只含路由给它的设备类）。
    pub const DEVICE_DIRECTORY: usize = ROLE_HANDLES + 3;
    /// 根服务的服务路由目录（`service_directory`）：按协议名连接 netstack、fshost 等服务的当前实例。
    pub const SERVICES: usize = ROLE_HANDLES + 4;
    /// 根 Job 的只读观察句柄（`INSPECT | ENUMERATE | TRANSFER`）：可遍历任务树并查询统计，
    /// 不能终止、改名或读写任何任务（Fuchsia `RootJobForInspect` 的同等权限）。
    pub const ROOT_JOB_OBSERVER: usize = ROLE_HANDLES + 5;
}

/// 存储宿主：拥有全部块设备与 ext4d 的连接；向使用者交出块设备的全盘范围连接，并转发 ext4 挂载
/// （Fuchsia fshost）。
pub mod fshost {
    use super::ROLE_HANDLES;

    /// bootfs 中的映像路径。
    pub const IMAGE: &[u8] = b"bin/fshost";
    /// 静态链接基址（普通进程的低地址区）。
    pub const BASE: usize = 0x40_0000;
    /// outgoing 服务目录的服务端：接受 `service_directory::BLOCKS` 与 `EXT4` 连接。
    pub const OUTGOING: usize = ROLE_HANDLES;
    /// 只含块设备的设备目录连接。
    pub const DEVICE_DIRECTORY: usize = ROLE_HANDLES + 1;
    /// ext4d 控制 channel 的客户端。
    pub const EXT4: usize = ROLE_HANDLES + 2;
}

/// ext4 文件系统服务：按 fshost 转发的挂载请求打开块设备上的 ext4，并以 FUSE 线协议服务每个挂载。
pub mod ext4d {
    use super::ROLE_HANDLES;

    /// bootfs 中的映像路径。
    pub const IMAGE: &[u8] = b"bin/ext4d";
    /// 静态链接基址（普通进程的低地址区）。
    pub const BASE: usize = 0x40_0000;
    /// 控制 channel 的服务端。
    pub const CONTROL: usize = ROLE_HANDLES;
}

/// 网络服务：拥有 IPv4 协议栈、packet socket 与默认网络；netcfg 安装网卡，使用者创建网络域与 socket。
pub mod netstack {
    use super::ROLE_HANDLES;

    /// bootfs 中的映像路径。
    pub const IMAGE: &[u8] = b"bin/netstack";
    /// 静态链接基址（普通进程的低地址区）。
    pub const BASE: usize = 0x40_0000;
    /// outgoing 服务目录的服务端：接受 `service_directory::NETWORK` 与 `NETWORK_INSTALLER` 连接。
    pub const OUTGOING: usize = ROLE_HANDLES;
}

/// 驱动宿主：平台设备发现、全部设备驱动与设备目录的发布者。
pub mod driver_host {
    use super::ROLE_HANDLES;

    /// bootfs 中的映像路径。
    pub const IMAGE: &[u8] = b"bin/driver-host";
    /// 静态链接基址（普通进程的低地址区，与根服务不重叠）。
    pub const BASE: usize = 0x40_0000;
    /// 根 resource（切出 MMIO、IRQ resource，创建 IOMMU）。
    pub const RESOURCE: usize = ROLE_HANDLES;
    /// 平台 DTB VMO。
    pub const DEVICE_TREE: usize = ROLE_HANDLES + 1;
    /// 设备目录的服务端：driver-host 以全部设备类服务它，根服务持有客户端并按类派生给各使用者。
    pub const DEVICE_DIRECTORY: usize = ROLE_HANDLES + 2;
}

/// 根服务。
pub mod root_server {
    /// 静态链接基址（普通进程的低地址区）。
    pub const BASE: usize = 0x20_0000;
}

/// 网络配置：发现网卡并经 netstack 的安装协议安装，按默认策略与用户设置配置接口（DHCP 经 dhcp-client），
/// 提供网络设置协议（Fuchsia netcfg）。
pub mod netcfg {
    use super::ROLE_HANDLES;

    /// bootfs 中的映像路径。
    pub const IMAGE: &[u8] = b"bin/netcfg";
    /// bootfs 中的只读默认策略（interfaces(5) 子集，`iface *` 匹配未单独配置的以太网口）。
    pub const DEFAULT_CONFIG: &[u8] = b"data/netcfg/default.conf";
    /// 静态链接基址（普通进程的低地址区）。
    pub const BASE: usize = 0x40_0000;
    /// outgoing 服务目录的服务端：接受 `service_directory::NETWORK_SETTINGS` 连接。
    pub const OUTGOING: usize = ROLE_HANDLES;
    /// 根服务的服务路由目录（netstack 控制与安装、dhcp-client）。
    pub const SERVICES: usize = ROLE_HANDLES + 1;
    /// 只含网卡的设备目录连接。
    pub const DEVICE_DIRECTORY: usize = ROLE_HANDLES + 2;
    /// fshost 派生的私有数据目录（`filesystem_protocol::Storage`）：持久化用户设置。
    pub const STORAGE: usize = ROLE_HANDLES + 3;
    /// 默认策略的只读 VMO（内容即 [`DEFAULT_CONFIG`]）。
    pub const CONFIG: usize = ROLE_HANDLES + 4;
}

/// DHCPv4 客户端：经 netstack 的 packet socket 为 netcfg 指定的接口取得并续租地址（Fuchsia dhcp-client）。
pub mod dhcp_client {
    use super::ROLE_HANDLES;

    /// bootfs 中的映像路径。
    pub const IMAGE: &[u8] = b"bin/dhcp-client";
    /// 静态链接基址（普通进程的低地址区）。
    pub const BASE: usize = 0x40_0000;
    /// outgoing 服务目录的服务端：接受 `service_directory::DHCP` 连接。
    pub const OUTGOING: usize = ROLE_HANDLES;
    /// 根服务的服务路由目录（netstack 控制）。
    pub const SERVICES: usize = ROLE_HANDLES + 1;
}
