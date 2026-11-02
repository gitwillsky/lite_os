# 领域依赖说明

精确的 kernel DAG 由[全局契约](../architecture-contract.md)维护。此处解释内核机制与服务之间的依赖方向。

## 内核机制

- `arch` 拥有 ISA、执行上下文、trap 编解码、页表编码、ASID、MMIO 与 IOMMU 寄存器机制；只经编译期 façade 选择后端。
- `platform` 拥有 machine/firmware、DTB 中的 CPU/内存/中断控制器/串口事实、早期串口、GIC/PLIC 与跨 CPU 同步。它不枚举通用总线或装配用户态驱动。
- `execution::entry` 只解码 boot/trap raw ABI；`execution::cpu` 独占 hardware→logical identity 与 CPU 生命周期。
- `execution::sync` 拥有内核 IRQ-safe/可睡眠同步；`execution::deferred` 合并内核延后工作；`execution::timer` 提供单调时钟与本地 timer。它们不拥有服务的工作队列。
- `serial` 独占串口 RX ring；platform 只发布字节，native console 读取它。`log` 独占日志输出与 ring，不作为串口输入的中转 owner。
- `memory` 只拥有 frame、地址类型和堆分配。`vm` 拥有 VMO、VMAR、页表帧、translation commit 与 pager 请求，不理解进程映像、文件、VMA 或 errno。
- `iommu` 独占 domain、IOVA 与固定页生命周期；`arch::dma` 提供具体翻译硬件。泛型 `DmaFrame` 由上层 frame owner 实现，后端不反向依赖 VM、对象或 platform。
- `thread` 独占 native 线程、调度状态、CPU 时间、等待 membership 和上下文切换。`object` 拥有句柄、Job/Process、channel/port/event、pager、resource/BTI 等原生对象。
- `syscall` 解码 native ABI、user-copy 和对象操作；`bootstrap` 从 bootfs 载入唯一 root-server。`main` 只装配，`crash` 只处理内核 fail-stop 与持久现场。

## 用户态服务

| Owner | 独占事实 | 依赖与边界 |
|---|---|---|
| `system/root-server` | 服务启动顺序、初始 capability 分配 | native-runtime、launch-protocol；不执行 Linux init 或设备协议 |
| `system/driver-host` | 设备 DTB、PCI、VirtIO、xHCI/UAC2、IRQ 完成线程、DMA、启动画面 | resource/IRQ/BTI native 对象；经 device-protocol 发布设备 |
| `system/ext4d` | ext4 inode、extent、journal、块设备连接 | device-protocol 与 FUSE 服务；不拥有调用者凭据、Linux FD 或进程权限 |
| `system/netstack` | smoltcp、IPv4 FIB/地址、端口、TCP/UDP/RAW、AF_PACKET 队列 | device-protocol 与 network-protocol；不拥有 Linux syscall、信号或阻塞超时 |
| `system/linuxd` | Linux ABI、进程/线程/namespace、安全策略、VFS/页缓存、设备 UAPI、Linux IPC | restricted mode 执行应用；经服务协议访问设备、ext4 与网络 |

`native-runtime` 封装原生对象和服务线程；`service-sync` 提供服务可睡眠同步；`service-rpc` 提供服务 RPC。
`device-model` 定义设备类词汇与 backing；各 `*-protocol` crate 拥有服务消息和客户端/服务端适配。
它们是系统服务接口；Linux 应用的内核入口只使用 Linux UAPI。服务不得直接链接另一个服务的 binary 源码。
`task-observer-protocol` 是桌面系统工具的只读任务观测消息 owner；仅定义数据，不依赖 native ABI、Linuxd 或 UI。原生权限与观测契约见[执行契约](execution.md#原生任务观测)。

FUSE 线格式属于 ext4d 的服务适配边界，不把 Linux 凭据策略带进 ext4 引擎。

## 验证职责

结构检查只约束真实 owner、依赖、scoped interface、unsafe/全局状态、ABI 和静态硬件边界。
不再以旧函数名、已删除的 swap 字段或“内核驱动 bootstrap WFI”证明服务行为，也不把源码分支乘固定次数当作实测成本。
资源寿命、并发和协议由实际生产 owner fixture 验证；IPC、restricted mode 与设备硬件由冷启动 guest 验证。
覆盖路由见[构建与验证](../development/build-and-verify.md)。
