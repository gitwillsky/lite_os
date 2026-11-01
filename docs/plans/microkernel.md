# 微内核迁移计划

Status: Active

本计划只在迁移执行期间存在；完成后把持久事实迁入 `docs/architecture*`、契约与 ABI 矩阵，然后删除本文件。

## 决策（2026-11-01 确认）

1. 终态采用 Fuchsia 形态：内核只提供通用机制，内核代码与文档中不出现 Linux 概念。
2. Linux ABI 由用户态 `linuxd` 实现（Starnix 模型）：Linux 程序在 restricted mode 执行，`linuxd` 在同一线程的 normal mode 处理其 syscall 与异常。
   应用继续以 Linux 64-bit ABI 二进制运行，不重新编译。
3. 自顶向下迁移：先建通用内核核心，再把现有 Linux 实现整体**移动**（不复制）到 `linuxd`，最后从 `linuxd` 拆出独立服务。
4. 内核原生对象接口（`native-abi`）是系统服务接口，不是应用 ABI；restricted mode 的 syscall 指令总是退出到 normal mode，Linux 程序无法调用它。
5. SMMUv3 与 RISC-V IOMMU 纳入范围；AArch64 与 RISC-V64 同步实现全部新机制。
6. 实现期间只做静态 review；整个方案全部实现完成后再整体验证，I/O 类门禁允许按新实现重新标定（需附测量证据）。

## Fuchsia 的通用机制（参照）

| 机制 | Zircon | Starnix 的用法 |
|---|---|---|
| restricted mode | `zx_restricted_enter`/`bind_state`；restricted 代码的 syscall、未解决异常或 kick 使内核把寄存器存入 state VMO，带原因码跳回 normal mode 入口 | Linux syscall、fault→signal、signal frame |
| 共享进程 | `zx_process_create_shared`：共享区 + 每进程私有区 | 所有 Linux 进程共享 Starnix 映像，私有区放 Linux 程序 |
| VMO/VMAR | 内存对象、COW child、pager | `mmap`、`fork` COW、文件页缓存 |
| futex | 按地址等待/唤醒 | Linux futex 语义 |
| kick | 强制线程退出 restricted mode | 投递 signal |
| 线程/进程/Job | 通用执行体与资源树 | Linux pid、线程组、凭据、namespace 都是 Starnix 数据结构 |

## Rust 抽象策略

- 内核对象公共行为由 `KernelObject` trait 统一（类型、默认 rights、signal、等待取消）；句柄 payload 是封闭 enum，只用于取得类型化引用。
- 内核内部不使用 async：调度器阻塞等待是唯一等待机制。
- 用户态系统服务以 async 为默认模型：`native-runtime` 提供以 port 为 reactor 的 executor，`Channel::recv`、`on_signals` 等返回 `Future`。
  `linuxd` 处理 Linux syscall 时与 Starnix 相同，在该 Linux 线程自己的 native 线程上同步执行；后台工作（网络、I/O completion）使用 async。
- IPC 协议以 Rust trait 描述请求/回复，client 与 server 共享协议 crate。

## 目标分层

```
Linux 程序（restricted mode）          BusyBox / Node / compositor / desktop
──────── svc / fault / kick → 退出到 normal mode ────────
linuxd（normal mode，共享区）           现 syscall、task、file、fs、ipc、net、namespace、io_uring、
                                       devices 的 UAPI 部分
系统服务                                root-server、devmgr、virtio/pci/usb 驱动、netd、ext4d、gpud …
════════════════════ native-abi ════════════════════
内核                                    arch、platform、cpu、sync、timer、deferred、interrupt、
                                       frame/page table、object、thread、vm、futex、restricted、
                                       iommu、crash、log、boot
```

## 现有模块去向

| 现有领域 | 去向 | 说明 |
|---|---|---|
| `arch`、`platform`、`cpu`、`sync`、`timer`、`deferred`、`config`、`fallible_tree`、`id`、`log`、`lang_item`、`crash` | 内核 | `arch` 增加 restricted mode 进入/退出与 state 布局 |
| `memory` 的 frame allocator、page table、kernel heap、TLB shootdown | 内核 `vm` | 改为服务 VMAR/VMO |
| `memory` 的 `MemorySet`/VMA/brk/RLIMIT/swap 策略 | `linuxd` 的 Linux mm | 落到 VMAR/VMO 调用上 |
| `execution::task` 的调度器、processor、kernel stack、上下文切换、等待协议 | 内核 `thread` | 抽出为不含 Linux 概念的线程核心 |
| `execution::task` 的 Process/Thread/signal/ptrace/seccomp/wait/pidfd | `linuxd` | Linux 线程 = restricted mode 下的 native 线程 |
| `syscall`、`file`、`fs`、`ipc`、`net`、`namespace`、`io_uring`、`system` | `linuxd` | 先整体移入，P3 再拆出 ext4d、netd |
| `devices::{tty,drm,input,audio,virtio_port,usb}` 的 UAPI | `linuxd` | 设备专属 ioctl 转发给对应服务 |
| `drivers::{virtio,pci,usb}`、`devices::{model,block,console}` | 用户态驱动 | P2 先作为 `linuxd` 内模块，P3 拆为驱动服务 |
| `random` | 内核 CPRNG | 通过 `cprng_draw` 提供；熵源驱动在用户态 |
| `boot::boot_screen` | 用户态 | 由 gpud 绘制；内核只保留 crash 显示所需的 framebuffer 授权 |

## 阶段

### P1 通用内核核心（已完成：两架构 clippy 通过；Linux 领域源码已移入 `system/linuxd/src`，待 P2 移植）

1. `native-abi`：调用编号、Status、rights、signals、packet、restricted state 布局。
2. `object`：句柄表、Channel、Port、Event、VMO、Process/Thread trait。
3. `thread`：从 `execution::task` 抽出调度实体、kernel stack、上下文切换与等待协议；Linux TCB 改为它的客户端，scheduler 不再引用 Linux 类型。
4. `vm`：VMAR（地址区间树、权限、映射）、VMO（匿名、COW child、物理连续、pager-backed），按 VMO/VMAR 做缺页。
5. 原生进程/线程/Job：Job 资源树与 kill 传播；进程 TERMINATED 时关闭句柄表。
6. futex：私有（地址空间, VA）与共享（VMO, 偏移）两种 key。
7. restricted mode：`restricted_bind_state`/`enter`/`kick`；共享进程；缺页先按 VMAR 解决，不能解决则以 EXCEPTION 原因退出。
8. 异常 channel：native 进程未处理异常的报告与终止。
9. 驱动原语：Interrupt 对象（含 wait/ack）、MMIO resource、BTI（DMA pin）、IOMMU（SMMUv3、RISC-V IOMMU）。
10. 启动：initrd 交付 cpio `newc` bootfs，内核加载固定的 `bin/root-server`，经 bootstrap channel 交付 bootfs VMO、root job 与 resource。
11. `cprng_draw`、`clock_get`、`debug_write`、crash framebuffer 授权。

P1 已知限制（P4 迁入架构文档的当前限制）：

- 内核蓝屏已删除，panic 只走串口；图形化 crash 显示需在 gpud 就绪后由用户态重新提供（待用户决策）。
- SMMUv3 使用线性 stream table（≤ 4096 stream），RISC-V IOMMU 使用两级设备目录；两者都未启用事件/故障队列。
- 内核不做页面换出；内存压力与换出策略属于 pager 服务。
- 内核 user-copy 只在 syscall 参数上做缺页解析；`linuxd` 访问 Linux 程序内存使用共享地址空间直接访问或 `process_read/write_memory`。

### P2 linuxd（移动现有 Linux 实现）

1. `system/` workspace：`native-runtime`（入口、分配器、panic、syscall 封装、async executor）、`root-server`。
2. `linuxd`：把 P1 未留下的 Linux 领域移入，把内核 API 调用改成 native 调用：MemorySet→VMAR/VMO，TaskMutex/TaskEvent→futex/port，trap→restricted exit 循环，user-copy→直接访问共享地址空间中的 restricted 区。
3. 驱动以 `linuxd` 内模块运行，使用 Interrupt/MMIO/BTI。
4. 切换：root-server 启动 `linuxd`，`linuxd` 挂载根文件系统并启动 `/sbin/init`；删除内核中全部 Linux 领域及其契约与 architecture-check 规则。

### P3 拆分服务

1. devmgr 与驱动服务（virtio-blk/net/gpu/input/sound/console/rng、PCI、xHCI/USB）。
2. `ext4d` 作为 pager，`linuxd` 经 FUSE 协议访问。
3. `netd`（smoltcp），`linuxd` 的 INET socket 转发给它。
4. 设备 UAPI 由 `linuxd` 转发给对应服务。

### P4 收尾与整体验证

1. 文档 owner 迁移：架构、契约、ABI 矩阵与构建说明改写为终态。
2. architecture-check：内核依赖矩阵不含 Linux 领域；新增“内核不得出现 Linux 概念”的静态规则。
3. 整体验证：native 门禁、全部 Linux 运行时门禁、性能重新标定；之前各阶段不运行门禁。

## 完整性清单（实现期间静态 review 对照）

- 每个移入 `linuxd` 的领域：所有调用方改为 native 调用，内核侧旧路径与契约条目同时删除。
- 每个内核对象：创建、句柄传递、关闭、取消等待、进程终止时的清理路径都已实现。
- 每个阻塞点：能被进程终止或 kick 打断，不会留下 observer 或预留空位。
- 两个架构后端：restricted mode、state 布局、上下文切换、IOMMU 均有实现。
- 现有 ABI 矩阵中已支持的每个 syscall 在 `linuxd` 中保持相同语义。
