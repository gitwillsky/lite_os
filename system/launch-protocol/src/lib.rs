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

/// 网络服务：拥有 IPv4 协议栈、packet socket 与默认网络；router_config 安装网卡，使用者创建网络域与 socket。
pub mod netstack {
    use super::ROLE_HANDLES;

    /// bootfs 中的映像路径。
    pub const IMAGE: &[u8] = b"bin/netstack";
    /// 静态链接基址（普通进程的低地址区）。
    pub const BASE: usize = 0x40_0000;
    /// outgoing 服务目录的服务端：接受 `service_directory::NETWORK` 与 `NETWORK_INSTALLER` 连接。
    pub const OUTGOING: usize = ROLE_HANDLES;
    /// 只覆盖 system::SCHEDULE，用于为转发 worker 分配独占核。
    pub const SCHEDULER: usize = ROLE_HANDLES + 1;
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
    /// 根服务持有设备目录及使用者连接；驱动管理器持发布端。
    pub const DEVICE_PUBLISHER: usize = ROLE_HANDLES + 2;
    /// 驱动宿主所在的 Job。
    pub const DRIVER_JOB: usize = ROLE_HANDLES + 3;
    /// 驱动宿主映像（bootfs 中 [`super::driver_host::IMAGE`] 的只读副本）。
    pub const DRIVER_HOST_IMAGE: usize = ROLE_HANDLES + 4;
    /// 根服务与管理器间的生命周期控制 channel。
    pub const CONTROL: usize = ROLE_HANDLES + 5;
    /// 切换时由旧管理器发给新管理器的节点状态流，冷启动为无效句柄。
    pub const HANDOVER: usize = ROLE_HANDLES + 6;
    /// 切换决策，USER_0 提交，USER_1 撤销；冷启动为无效句柄。
    pub const DECISION: usize = ROLE_HANDLES + 7;
}

/// 驱动更新控制面的版本、请求格式及资源上限。
pub mod driver_update {
    /// Linux 管理员通过 Unix datagram 发送请求，并用 SCM_RIGHTS 交付已打开的 ELF。
    pub const PATH: &[u8] = b"/dev/driver-update";
    /// 固定请求前缀。后接一个小端 u64，0 表示管理器，否则为 MMIO 基址或 PCI 配置空间地址。
    pub const VERSION: &[u8; 8] = b"LDRVUPD1";
    /// 映像传输上限，约束常驻的升级与回退资源。
    pub const IMAGE_LIMIT: u64 = 16 * 1024 * 1024;
    /// 单阶段设备初始化期限，单位纳秒。
    pub const START_TIMEOUT: i64 = 10_000_000_000;

    /// ELF 生命周期 note 类型，标识驱动宿主。
    pub const HOST_NOTE: u32 = 1;
    /// ELF 生命周期 note 类型，标识驱动管理器。
    pub const MANAGER_NOTE: u32 = 2;
    /// 宿主启动与设备协议、管理器启动与交接协议的共同兼容代际。
    pub const ABI: u32 = 5;

    /// 生成独立 PT_NOTE，依次为 ELF note header、NUL 终止的 owner 及 ABI 代际。
    pub const fn note(role: u32) -> [u8; 24] {
        let mut bytes = *b"\x07\0\0\0\x04\0\0\0\0\0\0\0LiteOS\0\0\0\0\0\0";
        let role = role.to_le_bytes();
        let version = ABI.to_le_bytes();
        let mut byte = 0;
        while byte < 4 {
            bytes[8 + byte] = role[byte];
            bytes[20 + byte] = version[byte];
            byte += 1;
        }
        bytes
    }

    /// 更新结果和回退结果各占一个小端 i32。回退未执行或成功时第二项为 0。
    pub fn reply(update: i32, rollback: i32) -> [u8; 8] {
        let mut bytes = [0; 8];
        bytes[..4].copy_from_slice(&update.to_le_bytes());
        bytes[4..].copy_from_slice(&rollback.to_le_bytes());
        bytes
    }
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
    /// 宿主绑定及 DMA 隔离清理完成后置位 USER_0。
    pub const READY: usize = ROLE_HANDLES + 2;
    /// 转发 OPEN 的独立设备目录连接，避免独占会话的回收阻塞监督。
    pub const OPEN_DIRECTORY: usize = ROLE_HANDLES + 3;
    /// 可变节点 resource 起点：按 IRQ（若有）、MSI（若有）、MMIO 窗口顺序交付。
    pub const RESOURCES: usize = ROLE_HANDLES + 4;

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
        /// source）、声音（audio-core 把旧输出标为断开、把新设备接为新输出，linuxd 呈现为声卡重新插入）、
        /// 输入（linuxd 撤下 evdev 节点并发 remove/add uevent）、console 端口（linuxd 以新会话重新发布节点）
        /// 网卡（router_config 重新安装并配置网卡），以及 USB 主控（旧 USB 与 PCM 会话断开，主控复位后重新枚举端口）。
        /// 块设备和显示仍缺少消费者恢复契约，宿主崩溃时节点被移除。
        pub const fn restart_on_crash(self) -> bool {
            matches!(
                self,
                Self::VirtioEntropy
                    | Self::VirtioSound
                    | Self::VirtioInput
                    | Self::VirtioConsole
                    | Self::VirtioNetwork
                    | Self::Xhci
            )
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
            /// msi-map 解析的消息 DeviceID；对应独立 MSI resource，不能与 DMA stream 混用。
            message_device: Option<u32>,
        },
    }

    /// 驱动宿主的启动参数。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Node {
        pub driver: Driver,
        pub bus: Bus,
    }

    /// 编码后的长度。
    const INTERRUPT_OFFSET: usize = 4 + 4 + 8 + 6 * 16;
    const MESSAGE_OFFSET: usize = INTERRUPT_OFFSET + 4;
    pub const NODE_BYTES: usize = MESSAGE_OFFSET + 8;
    const NO_INTERRUPT: u32 = u32::MAX;

    impl Node {
        /// 启动资源中是否包含 IRQ。
        pub fn has_irq(&self) -> bool {
            match self.bus {
                Bus::VirtioMmio { .. } => true,
                Bus::Pci { interrupt, .. } => interrupt.is_some(),
            }
        }

        /// 启动资源中是否包含单设备 MSI authority。
        pub fn has_msi(&self) -> bool {
            matches!(
                self.bus,
                Bus::Pci {
                    message_device: Some(_),
                    ..
                }
            )
        }

        /// 节点的运行时定位地址。
        pub fn address(&self) -> u64 {
            match self.bus {
                Bus::VirtioMmio { base, .. } => base,
                Bus::Pci { config, .. } => config,
            }
        }

        /// 编码为启动参数。
        pub fn encode(&self) -> [u8; NODE_BYTES] {
            let mut bytes = [0u8; NODE_BYTES];
            bytes[0..4].copy_from_slice(&(self.driver as u32).to_le_bytes());
            match self.bus {
                Bus::VirtioMmio { base, size, irq } => {
                    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
                    bytes[8..16].copy_from_slice(&base.to_le_bytes());
                    bytes[16..24].copy_from_slice(&size.to_le_bytes());
                    bytes[INTERRUPT_OFFSET..INTERRUPT_OFFSET + 4]
                        .copy_from_slice(&irq.to_le_bytes());
                }
                Bus::Pci {
                    config,
                    bars,
                    interrupt,
                    message_device,
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
                    bytes[INTERRUPT_OFFSET..INTERRUPT_OFFSET + 4]
                        .copy_from_slice(&interrupt.unwrap_or(NO_INTERRUPT).to_le_bytes());
                    if let Some(device) = message_device {
                        bytes[MESSAGE_OFFSET..MESSAGE_OFFSET + 4]
                            .copy_from_slice(&1u32.to_le_bytes());
                        bytes[MESSAGE_OFFSET + 4..].copy_from_slice(&device.to_le_bytes());
                    }
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
            let tail = u32_at(INTERRUPT_OFFSET);
            let message_device = match u32_at(MESSAGE_OFFSET) {
                0 if u32_at(MESSAGE_OFFSET + 4) == 0 => None,
                1 => Some(u32_at(MESSAGE_OFFSET + 4)),
                _ => return None,
            };
            if u32_at(4) != 2 && message_device.is_some() {
                return None;
            }
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
                        message_device,
                    }
                }
                _ => return None,
            };
            Some(Self { driver, bus })
        }
    }
}

/// 音频服务：拥有 PCM 输出设备与混音，经音频协议向使用者提供渲染流（Fuchsia audio_core）。
pub mod audio_core {
    use super::ROLE_HANDLES;

    /// bootfs 中的映像路径。
    pub const IMAGE: &[u8] = b"bin/audio-core";
    /// 静态链接基址（普通进程的低地址区）。
    pub const BASE: usize = 0x40_0000;
    /// outgoing 服务目录的服务端：接受 `service_directory::AUDIO` 连接。
    pub const OUTGOING: usize = ROLE_HANDLES;
    /// 只含 PCM 输出的设备目录连接（含热插拔的 USB 声卡）。
    pub const DEVICE_DIRECTORY: usize = ROLE_HANDLES + 1;
    /// 只覆盖 system::SCHEDULE 的资源，用于混音线程的截止预算。
    pub const SCHEDULER: usize = ROLE_HANDLES + 2;
}

/// 根服务。
pub mod component_manager {
    /// 静态链接基址（普通进程的低地址区）。
    pub const BASE: usize = 0x20_0000;
}

/// 由路由配置 owner 激活的原生 DNS 转发服务。
pub mod dns_cache {
    use super::ROLE_HANDLES;
    /// Bootfs 中的 DNS 执行服务。
    pub const IMAGE: &[u8] = b"bin/dns-cache";
    /// 独立进程的静态链接基址。
    pub const BASE: usize = 0x40_0000;
    /// 仅发布执行会话工厂的目录。
    pub const OUTGOING: usize = ROLE_HANDLES;
    /// 只包含原生网络控制服务的路由目录。
    pub const SERVICES: usize = ROLE_HANDLES + 1;
}

/// BGP 原生协议服务；配置工厂只委托给配置 owner，实例能力关闭即停止相应路由域。
pub mod bgp {
    use super::ROLE_HANDLES;
    /// bootfs 中的 BGP 服务映像。
    pub const IMAGE: &[u8] = b"bin/bgp";
    /// 独立进程的静态链接基址。
    pub const BASE: usize = 0x40_0000;
    /// 只提供 BGP 配置工厂的 outgoing 目录。
    pub const OUTGOING: usize = ROLE_HANDLES;
}

/// 独立的 OSPF 协议服务，由组件管理器监督。
pub mod ospf {
    use super::ROLE_HANDLES;
    /// bootfs 中的 OSPF 服务映像。
    pub const IMAGE: &[u8] = b"bin/ospf";
    /// 独立进程的静态链接基址。
    pub const BASE: usize = 0x40_0000;
    /// 仅发布 OSPF 实例工厂的目录。
    pub const OUTGOING: usize = ROLE_HANDLES;
}

/// 原生 RIB：接管受授权配置来源，选择候选并委托 netstack 执行。
pub mod routing_rib {
    use super::ROLE_HANDLES;
    /// bootfs 中的原生 RIB 映像。
    pub const IMAGE: &[u8] = b"bin/routing-rib";
    /// 独立进程的静态链接基址。
    pub const BASE: usize = 0x40_0000;
    /// 提供获授权配置来源的 outgoing 目录。
    pub const OUTGOING: usize = ROLE_HANDLES;
    /// 只含 NET 控制与执行能力的路由目录。
    pub const SERVICES: usize = ROLE_HANDLES + 1;
}

/// 网络配置树与持久决定 owner。
pub mod router_config {
    use super::ROLE_HANDLES;

    /// bootfs 中的映像路径。
    pub const IMAGE: &[u8] = b"bin/router-config";
    /// bootfs 中的只读默认策略（interfaces(5) 子集，`iface *` 匹配未单独配置的以太网口）。
    pub const DEFAULT_CONFIG: &[u8] = b"data/router-config/default.conf";
    /// 已有数据卷上的稳定存储身份，与进程展示名独立；改名不能静默丢弃既存网络设置。
    pub const STORAGE_ID: &[u8] = b"netcfg";
    /// 静态链接基址（普通进程的低地址区）。
    pub const BASE: usize = 0x40_0000;
    /// outgoing 服务目录的服务端：接受 `service_directory::NETWORK_SETTINGS` 连接。
    pub const OUTGOING: usize = ROLE_HANDLES;
    /// 根服务的服务路由目录（netstack 控制、RIB 接口来源与 dhcp-client）。
    pub const SERVICES: usize = ROLE_HANDLES + 1;
    /// 只含网卡的设备目录连接。
    pub const DEVICE_DIRECTORY: usize = ROLE_HANDLES + 2;
    /// fshost 派生的私有数据目录（`filesystem_protocol::Storage`）：持久化用户设置。
    pub const STORAGE: usize = ROLE_HANDLES + 3;
    /// 默认策略的只读 VMO（内容即 [`DEFAULT_CONFIG`]）。
    pub const CONFIG: usize = ROLE_HANDLES + 4;
}

/// DHCPv4 客户端：经 netstack 的 packet socket 为 router_config 指定的接口取得并续租地址（Fuchsia dhcp-client）。
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

/// 下游 DHCPv4 地址分发，由 router-config 按接口开启。
pub mod dhcp_server {
    use super::ROLE_HANDLES;

    /// bootfs 中的映像路径。
    pub const IMAGE: &[u8] = b"bin/dhcp-server";
    /// 静态链接基址。
    pub const BASE: usize = 0x40_0000;
    /// 服务目录的提供端。
    pub const OUTGOING: usize = ROLE_HANDLES;
    /// 根服务路由目录，只包含网络控制能力。
    pub const SERVICES: usize = ROLE_HANDLES + 1;
}
