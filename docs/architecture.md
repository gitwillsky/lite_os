# LiteOS 当前架构

LiteOS 使用微内核与用户态服务，服务拓扑对齐 Fuchsia。内核提供通用对象、线程调度、虚拟内存、中断与设备隔离机制；
Linux 应用以原有二进制在 restricted mode 执行，由可插拔的用户态 `linuxd` 处理 Linux syscall、信号与进程语义。
AArch64 和 RISC-V64 共用这些机制，具体指令与 machine 装配通过编译期静态 façade 选择。

## 边界与启动

1. `kernel/src/main.rs` 装配内核机制；`bootstrap` 从 bootfs 装载唯一的 `component-manager`，交付根 Job、
   resource、bootfs 与 DTB。内核不解释服务目录、文件系统、网络和应用启动策略。
2. `system/component-manager` 是能力路由与监督者（对应 Fuchsia component_manager）：启动各服务，按设备类派生
   自身持有的设备目录（Network → router-config，Block → fshost，Pcm → audio-core，其余 → linuxd），并为每个消费者服务
   只含其声明协议的路由目录。netstack、routing-rib、dhcp-client、router-config、audio-core 与 linuxd 终止后重启，崩溃循环时关机；
   当前 driver-manager 或 fshost 意外终止时关机。规则见[依赖与 owner 契约](architecture-contract/dependency-owners.md#服务拓扑与能力路由)。
3. `driver-manager` 拥有平台事实与节点拓扑，为每个节点启动一个 `driver-host`（Fuchsia DFv2）。根服务的设备目录及宿主 Job
   跨管理器换版保留，设备会话由宿主持有。驱动换版与管理器交接见[设备契约](architecture-contract/devices-terminal.md#运行时换版)。`fshost` 拥有块设备会话并为每块盘启动 ext4 实例，交出 Linux 块设备、
   按盘挂载 ext4，并在原生数据卷上为原生服务派生私有目录；ext4 实例拥有普通文件内容（pager VMO）。
   `audio-core` 拥有 PCM 输出与混音，linuxd 只翻译 ALSA。
   `netstack` 提供协议栈与 FIB 执行；`router-config` 发现网卡，经原生 RIB 安装进默认网络并提交接口意图；RIB 选择默认路由，
   DHCP 由 `dhcp-client` 完成。
4. `linuxd` 是可插拔的 Linux 容器（Starnix 模型），只消费路由给它的能力：挂载根文件系统并启动 PID1；它退出后
   component-manager 终止整个 Linux Job 再重启它，其他服务不受影响。每个 Linux 进程对应一个共享进程的成员：低地址区
   属于应用，高地址区共享 `linuxd` 映像、堆与服务栈。每个 Linux 线程有一个原生承载线程；restricted 使用不含服务区
   USER 映射的 private 页表根，syscall/未解决异常/kick 使其回到 normal 页表根的兼容服务。
5. `native-abi` 只连接内核与系统服务；Linux 应用的 syscall 总是先退出 restricted mode，不能调用该接口。
   应用继续只消费 Linux UAPI。设备 UAPI、文件描述符、namespace 与 errno 都由 `linuxd` 拥有；授予指定系统程序的
   原生服务协议经 linuxd 发布的授权端点（`/dev/task-observer`、`/dev/network-settings`）适配。
6. 每个复合状态有唯一 owner。跨服务只通过 channel、VMO 和明确的协议 crate 传递所有权或数据。

## 源码组织

| 位置 | 所有权 |
|---|---|
| `kernel/src/arch`、`platform` | 执行/MMU mechanism；firmware、CPU 与中断控制器装配 |
| `kernel/src/execution`、`thread` | raw entry、语义 trap、timer、同步、调度与等待 |
| `kernel/src/vm`、`memory`、`iommu` | VMO/VMAR、页表与退休；物理页/堆；DMA 地址翻译 |
| `kernel/src/object`、`syscall` | 句柄、rights、对象生命周期及 native 参数编解码 |
| `system/native-runtime`、`service-sync`、`service-rpc` | 服务入口、分配/线程/port reactor、可睡眠同步与 IPC codec |
| `system/component-manager`、`service-directory` | 服务启动、监督与能力路由；按协议名连接服务的路由协议 |
| `system/linuxd` | Linux ABI、VFS、进程/信号/安全、设备 UAPI（含 ALSA 状态机）、tmpfs 与 Linux FUSE 页缓存、应用装载与授权端点适配 |
| `system/driver-manager`、`driver-host`、`device-model`、`device-protocol` | 平台事实、节点匹配与宿主监督；每节点一个驱动宿主；设备类 trait、汇聚设备目录与可重开的设备会话 |
| `system/fshost`、`ext4`、`filesystem-protocol`、`pager-pool` | 块设备、数据卷与每盘 ext4 实例；ext4/JBD2、文件内容 VMO 与 FUSE 服务；卷挂载、块设备清单与存储能力协议；补页线程池 |
| `system/audio-core`、`audio-protocol` | PCM 输出、混音与主音量；输出列表、渲染流与共享控制区协议 |
| `system/netstack`、`network-protocol` | IPv4/transport/FIB 与网络域；控制、安装、域与 socket 协议 |
| `system/router-config`、`dhcp-client`、`router-protocol` | 接口配置策略与用户设置；DHCPv4 客户端；设置与租约编码 |
| `system/routing-rib` | 不可变候选与解析库；受授权接口来源、默认路由选择与原生执行协调 |
| `system/task-observer-protocol` | 原生系统服务观测的只读消息；KOID 不混入 Linux PID |
| `abi/native-abi`、`abi/linux-abi` | 系统服务接口与 Linux UAPI，边界分别固定 |
| `libraries/fallible-tree`、`libraries/mmio` | 唯一的可失败有序存储与有界 MMIO 访问实现 |
| `libraries/pci-config` | PCI 标准配置快照与 capability 格式校验；不拥有总线枚举、BAR 分配或中断路由 |
| `libraries/packet-wire` | 无状态报文字节算法；当前提供 RFC 1071 校验和，不拥有 socket、接口或 DMA |
| `libraries/boot-scene` | 启动画面位图的唯一解码与布局；driver-host 与 compositor 据此保证接管时逐像素一致 |
| `user` | Linux 应用、桌面、运行库与 UI SDK |

## 领域导航

各领域的设计与验证边界分别由[文档索引](README.md)路由；依赖、state owner 与 scoped interface
见[架构契约](architecture-contract.md)，应用可观察行为见[Linux ABI 矩阵](syscall-support.md)。
文档中的 kernel 仅指通用微内核；Linux task、VFS、socket 与设备 ioctl 的事实属于对应系统服务。

## 当前边界

- Apple Silicon 上的 AArch64/QEMU virt/HVF 是默认产品入口；TCG 必须显式选择。RISC-V64 使用 TCG，
  两个后端分别构建与验证，不能把一个后端的成功推定为另一个的成功。
- Linux ABI 是矩阵列出的子集，不等于完整 Linux/POSIX 或任意 Linux 应用兼容。
- 不做匿名页换出；后台回收线程按分配器水位回收未固定的干净 pager 页，用户缺页入口保留有界兜底。
  两后端可采集访问位并完成全 ASID fence；八代队列与有界扫描已通过 2 GiB 实例的 2.3 GiB 工作集诊断；负载、计数与边界见性能覆盖文档。相关 ABI 范围由内存 owner 声明。
- 服务使用无浮点 ABI；应用浮点状态跨 restricted 退出保留。对信号帧的显式读写由 `linuxd::arch` 负责。
- SMMUv3 与 RISC-V IOMMU 后端均存在；故障队列以轮询排空、只做诊断计数，硬件验证范围由平台契约声明。
- 驱动宿主崩溃后按[设备契约](architecture-contract/devices-terminal.md#运行时换版)重绑具有恢复路径的节点。
  网卡重建后是新接口（新 ifindex，名称取最小空闲 `eth<n>`），旧 TCP 连接不迁移。
- 缺失语义必须返回标准错误或保持接口不存在；不以 stub、忽略 flags 或放宽验收掩盖实现错误。
