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

/// 存储宿主：拥有全部块设备与文件系统服务实例；向使用者交出块设备的全盘范围连接，按盘序号挂载 ext4
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
    /// 文件系统服务实例所在的 Job（fshost 为每块盘启动一个 [`super::ext4`] 实例）。
    pub const FILESYSTEM_JOB: usize = ROLE_HANDLES + 2;
    /// ext4 映像（bootfs 中 [`super::ext4::IMAGE`] 的只读副本）。
    pub const EXT4_IMAGE: usize = ROLE_HANDLES + 3;
}

/// ext4 文件系统服务：fshost 为每块盘启动的实例；按挂载请求打开块设备上的 ext4，以 FUSE 线协议服务每个挂载。
pub mod ext4 {
    use super::ROLE_HANDLES;

    /// bootfs 中的映像路径。
    pub const IMAGE: &[u8] = b"bin/ext4";
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

/// 驱动管理：平台设备事实（DTB、PCI 枚举与 BAR 分配）、节点拓扑、驱动匹配与驱动宿主监督，
/// 并把各宿主发布的设备汇聚为一个设备目录（Fuchsia driver_manager + devfs）。
pub mod driver_manager {
    use super::ROLE_HANDLES;

    /// bootfs 中的映像路径。
    pub const IMAGE: &[u8] = b"bin/driver-manager";
    /// 静态链接基址（普通进程的低地址区）。
    pub const BASE: usize = 0x40_0000;
    /// 根 resource（为每个节点切出 MMIO、IRQ resource，创建 IOMMU 与 BTI）。
    pub const RESOURCE: usize = ROLE_HANDLES;
    /// 平台 DTB VMO。
    pub const DEVICE_TREE: usize = ROLE_HANDLES + 1;
    /// 设备目录的服务端：以全部设备类服务，根服务持有客户端并按类派生给各使用者。
    pub const DEVICE_DIRECTORY: usize = ROLE_HANDLES + 2;
    /// 驱动宿主所在的 Job。
    pub const DRIVER_JOB: usize = ROLE_HANDLES + 3;
    /// 驱动宿主映像（bootfs 中 [`super::driver_host::IMAGE`] 的只读副本）。
    pub const DRIVER_HOST_IMAGE: usize = ROLE_HANDLES + 4;
}

/// 驱动宿主：运行绑定到一个节点的驱动（及其同宿主的子驱动），在本进程的设备目录发布设备
/// （Fuchsia driver_host）。启动参数是编码后的 [`driver_host::Node`]。
pub mod driver_host {
    use super::ROLE_HANDLES;

    /// bootfs 中的映像路径。
    pub const IMAGE: &[u8] = b"bin/driver-host";
    /// 静态链接基址（普通进程的低地址区）。
    pub const BASE: usize = 0x40_0000;
    /// 本宿主设备目录的服务端：driver-manager 持有客户端并汇聚。
    pub const DIRECTORY: usize = ROLE_HANDLES;
    /// 节点的 DMA 发起者。
    pub const BTI: usize = ROLE_HANDLES + 1;
    /// 节点中断向量的 IRQ resource；节点没有中断时为无效句柄。
    pub const IRQ: usize = ROLE_HANDLES + 2;
    /// 节点 MMIO 窗口的 resource，从此下标起直到句柄末尾。
    pub const MMIO: usize = ROLE_HANDLES + 3;

    /// 绑定到节点的驱动。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(u32)]
    pub enum Driver {
        VirtioNetwork = 1,
        VirtioBlock = 2,
        VirtioConsole = 3,
        VirtioEntropy = 4,
        VirtioGpu = 5,
        VirtioInput = 6,
        VirtioSound = 7,
        /// xHCI 主控；USB 功能驱动与它同宿主（colocate）。
        Xhci = 8,
    }

    impl Driver {
        fn decode(value: u32) -> Option<Self> {
            Some(match value {
                1 => Self::VirtioNetwork,
                2 => Self::VirtioBlock,
                3 => Self::VirtioConsole,
                4 => Self::VirtioEntropy,
                5 => Self::VirtioGpu,
                6 => Self::VirtioInput,
                7 => Self::VirtioSound,
                8 => Self::Xhci,
                _ => return None,
            })
        }

        /// 驱动清单的 `host_restart_on_crash`：宿主意外终止后重建宿主并重绑节点。
        ///
        /// 只声明给使用者能在会话 `PEER_CLOSED` 后改用新登记设备的驱动：熵源（使用者总取最新的可用
        /// source）。其余驱动不声明，宿主崩溃时节点被移除：块设备（文件系统无法跨驱动实例恢复）、显示、
        /// USB 主控，以及使用者尚无设备移除语义的网卡（netstack 不撤下接口）、输入（evdev 节点不撤下）、
        /// 声音与 console 端口。
        pub const fn restart_on_crash(self) -> bool {
            matches!(self, Self::VirtioEntropy)
        }
    }

    /// 一个 PCI memory BAR。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Bar {
        pub address: u64,
        pub size: u64,
    }

    /// 节点所在总线及其已分配的资源坐标；对应的 resource 句柄随启动消息交付。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Bus {
        /// VirtIO-MMIO transport 窗口与中断向量。
        VirtioMmio { base: u64, size: u64, irq: u32 },
        /// 根总线 PCI function：配置空间页、已分配的 memory BAR 与 INTx 路由。
        Pci {
            config: u64,
            bars: [Option<Bar>; 6],
            interrupt: Option<u32>,
        },
    }

    /// 驱动宿主的启动参数。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Node {
        pub driver: Driver,
        pub bus: Bus,
    }

    /// 编码后的长度。
    pub const NODE_BYTES: usize = 4 + 4 + 8 + 6 * 16 + 4;
    const NO_INTERRUPT: u32 = u32::MAX;

    impl Node {
        /// 编码为启动参数。
        pub fn encode(&self) -> [u8; NODE_BYTES] {
            let mut bytes = [0u8; NODE_BYTES];
            bytes[0..4].copy_from_slice(&(self.driver as u32).to_le_bytes());
            match self.bus {
                Bus::VirtioMmio { base, size, irq } => {
                    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
                    bytes[8..16].copy_from_slice(&base.to_le_bytes());
                    bytes[16..24].copy_from_slice(&size.to_le_bytes());
                    bytes[NODE_BYTES - 4..].copy_from_slice(&irq.to_le_bytes());
                }
                Bus::Pci {
                    config,
                    bars,
                    interrupt,
                } => {
                    bytes[4..8].copy_from_slice(&2u32.to_le_bytes());
                    bytes[8..16].copy_from_slice(&config.to_le_bytes());
                    for (index, bar) in bars.iter().enumerate() {
                        let at = 16 + index * 16;
                        let bar = bar.unwrap_or(Bar {
                            address: 0,
                            size: 0,
                        });
                        bytes[at..at + 8].copy_from_slice(&bar.address.to_le_bytes());
                        bytes[at + 8..at + 16].copy_from_slice(&bar.size.to_le_bytes());
                    }
                    bytes[NODE_BYTES - 4..]
                        .copy_from_slice(&interrupt.unwrap_or(NO_INTERRUPT).to_le_bytes());
                }
            }
            bytes
        }

        /// 解码启动参数；格式或驱动未知时返回 `None`。
        pub fn decode(bytes: &[u8]) -> Option<Self> {
            if bytes.len() != NODE_BYTES {
                return None;
            }
            let u32_at =
                |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap_or([0; 4]));
            let u64_at =
                |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or([0; 8]));
            let driver = Driver::decode(u32_at(0))?;
            let tail = u32_at(NODE_BYTES - 4);
            let bus = match u32_at(4) {
                1 => Bus::VirtioMmio {
                    base: u64_at(8),
                    size: u64_at(16),
                    irq: tail,
                },
                2 => {
                    let mut bars = [None; 6];
                    for (index, bar) in bars.iter_mut().enumerate() {
                        let at = 16 + index * 16;
                        let size = u64_at(at + 8);
                        if size != 0 {
                            *bar = Some(Bar {
                                address: u64_at(at),
                                size,
                            });
                        }
                    }
                    Bus::Pci {
                        config: u64_at(8),
                        bars,
                        interrupt: (tail != NO_INTERRUPT).then_some(tail),
                    }
                }
                _ => return None,
            };
            Some(Self { driver, bus })
        }
    }
}

/// 根服务。
pub mod component_manager {
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
