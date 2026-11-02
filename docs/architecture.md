# LiteOS 当前架构

LiteOS 使用微内核与用户态兼容服务。内核提供通用对象、线程调度、虚拟内存、中断与设备隔离机制；
Linux 应用以原有二进制在 restricted mode 执行，由用户态 `linuxd` 处理 Linux syscall、信号与进程语义。
AArch64 和 RISC-V64 共用这些机制，具体指令与 machine 装配通过编译期静态 façade 选择。

## 边界与启动

1. `kernel/src/main.rs` 装配内核机制；`bootstrap` 从 bootfs 装载唯一的 `root-server`，交付根 Job、
   resource、bootfs 与 DTB。内核不解释服务目录、文件系统、网络和应用启动策略。
2. `system/root-server` 启动 `driver-host`、`ext4d`、`netstack` 和 `linuxd`，按角色交付句柄。
   `driver-host` 接收硬件根 resource，`linuxd` 只接收日志、串口、UTC 设置与电源的 system resource 范围。
   `driver-host` 拥有硬件驱动，`ext4d` 拥有 ext4 引擎，`netstack` 拥有协议栈；`linuxd` 挂载根文件系统并启动 PID1。
3. 每个 Linux 进程对应一个共享进程的成员：低地址区属于应用，高地址区共享 `linuxd` 映像、堆与服务栈。
   每个 Linux 线程有一个原生承载线程；restricted 使用不含服务区 USER 映射的 private 页表根，
   syscall/未解决异常/kick 使其回到 normal 页表根的兼容服务。
4. `native-abi` 只连接内核与系统服务；Linux 应用的 syscall 总是先退出 restricted mode，不能调用该接口。
   应用继续只消费 Linux UAPI。设备 UAPI、文件描述符、namespace 与 errno 都由 `linuxd` 拥有。
5. 每个复合状态有唯一 owner。跨服务只通过 channel、VMO 和明确的协议 crate 传递所有权或数据；
   不保留内核中的旧文件系统、协议栈和 Linux task 实现。

## 源码组织

| 位置 | 所有权 |
|---|---|
| `kernel/src/arch`、`platform` | 执行/MMU mechanism；firmware、CPU 与中断控制器装配 |
| `kernel/src/execution`、`thread` | raw entry、语义 trap、timer、同步、调度与等待 |
| `kernel/src/vm`、`memory`、`iommu` | VMO/VMAR、页表与退休；物理页/堆；DMA 地址翻译 |
| `kernel/src/object`、`syscall` | 句柄、rights、对象生命周期及 native 参数编解码 |
| `system/native-runtime`、`service-sync`、`service-rpc` | 服务入口、分配/线程/port reactor、可睡眠同步与 IPC codec |
| `system/linuxd` | Linux ABI、VFS、进程/信号/安全、设备 UAPI、pager 与应用装载 |
| `system/driver-host`、`device-model`、`device-protocol` | 驱动、设备类 trait、目录、设备协议与远端适配 |
| `system/ext4d`、`filesystem-protocol` | ext4/JBD2 与 FUSE 服务；挂载控制和大消息传输 |
| `system/netstack`、`network-protocol` | IPv4/transport/FIB 与网络域；跨服务 socket 操作与就绪事件 |
| `system/task-observer-protocol` | 原生系统服务观测的只读消息；KOID 不混入 Linux PID |
| `abi/native-abi`、`abi/linux-abi` | 系统服务接口与 Linux UAPI，边界分别固定 |
| `libraries/fallible-tree`、`libraries/mmio` | 唯一的可失败有序存储与有界 MMIO 访问实现 |
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
- 内核不提供 swap/reclaim 策略；当前用户态 pager 尚未提供换出服务。相关 syscall、procfs 与门禁的范围由内存 ABI owner 声明。
- 服务使用无浮点 ABI；应用浮点状态跨 restricted 退出保留。对信号帧的显式读写由 `linuxd::arch` 负责。
- SMMUv3 与 RISC-V IOMMU 后端均存在；设备故障/事件队列、硬件验证范围由平台契约声明。
- 缺失语义必须返回标准错误或保持接口不存在；不以 stub、忽略 flags 或放宽验收掩盖实现错误。
