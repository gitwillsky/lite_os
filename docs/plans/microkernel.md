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

- 内核与 linuxd 的蓝屏均已删除，panic 只走串口与崩溃记录；图形化 crash 显示若需要，由显示服务在用户态重新提供（待用户决策）。
- SMMUv3 使用线性 stream table（≤ 4096 stream），RISC-V IOMMU 使用两级设备目录；两者都未启用事件/故障队列。
- 内核不做页面换出；内存压力与换出策略属于 pager 服务。
- 内核 user-copy 只在 syscall 参数上做缺页解析；`linuxd` 访问 Linux 程序内存使用共享地址空间直接访问或 `process_read/write_memory`。

### P2 linuxd（已完成：两架构 clippy 通过；构建打包 bootfs 并经 `-initrd` 启动，待整体验证）

1. `system/` workspace：`native-runtime`（入口、分配器、panic、syscall 封装、async executor）、`root-server`。
2. `linuxd`：把 P1 未留下的 Linux 领域移入，把内核 API 调用改成 native 调用：MemorySet→VMAR/VMO，TaskMutex/TaskEvent→futex/port，trap→restricted exit 循环，user-copy→直接访问共享地址空间中的 restricted 区。
3. 驱动以 `linuxd` 内模块运行，使用 Interrupt/MMIO/BTI。
4. 切换：root-server 启动 `linuxd`，`linuxd` 挂载根文件系统并启动 `/sbin/init`；删除内核中全部 Linux 领域及其契约与 architecture-check 规则。

P2 运行形态（实现约束）：

- 系统服务（`system/`）按无浮点 ABI 编译（AArch64 `aarch64-unknown-none-softfloat`、RISC-V `riscv64imac-unknown-none-elf`），
  服务代码从不触碰浮点寄存器，Linux 程序的浮点状态跨 restricted 退出保持在硬件中；信号帧读写浮点状态时用内联汇编显式保存/恢复。
- 启动协议：`native_abi::startup`（`StartupHeader` + 参数，前三个句柄 SELF_PROCESS/SELF_VMAR/SELF_THREAD），角色句柄约定在
  `system/launch-protocol`。内核启动根服务同样遵循该协议。
- 共享进程：与 Zircon `ShareableProcessState` 一致，共享进程与成员共用句柄表；共享区地址的私有 futex key 属于共享空间。
- linuxd：映像链接在 `0x20_0000_0000`，装载在共享进程的共享区；堆与服务线程栈映射在共享区（SELF_VMAR 即共享 VMAR）。
  每个 Linux 进程是共享进程的成员，Linux 程序只使用私有低半区（Linux 可见的用户地址上限因此为 `SHARED_START`）。
- Linux 线程 = 成员进程中的 native 线程：绑定 restricted 状态后循环 `enter`；SYSCALL 退出时分派 Linux syscall，
  EXCEPTION 退出时转换为信号，KICK 退出时检查待处理信号。线程描述符的上下文字保存当前 TCB。
- linuxd 内的阻塞等待沿用迁移前的 `TaskWaitTarget` 协议：TCB 的 sleep/wake 落到线程 parker（futex），
  信号投递 = 置 pending + unpark + `restricted_kick`；deadline 由 futex 超时实现。
- Linux futex 由 linuxd 自己维护（按 mm 与地址/共享对象偏移索引），不使用内核 futex。
- user-copy：linuxd 线程运行在目标 Linux 进程的成员地址空间中，访问其他 Linux 进程内存使用 `process_read/write_memory`；
  当前进程内存同样经 `process_read/write_memory` 访问，以获得 EFAULT 语义而不是服务故障。

P2 linuxd 移植映射（旧内核机制 → linuxd）：

| 旧机制 | linuxd |
|---|---|
| `execution::sync`（TaskMutex/TaskEvent/RwSemaphore/TaskWaitTarget） | `linuxd::sync` 原样移入；`IrqMutex`/`spin::Mutex` 换为 futex `Mutex` |
| `processor` 的 RunState、runqueue、context switch | TCB 的执行状态 + native 线程 parker；`prepare_current_block`/`wake_*_task` 保持“发布精确 membership → 比较并消费”协议 |
| 阻塞截止时间（等待注册表 deadline + tick） | 后台 deferred 线程按注册表/定时器队列最早 deadline 停驻并到期唤醒 |
| `deferred` 软中断位图 | 同一 `DeferredWork` 位图，由单个后台线程消费 |
| group stop（Stopped/StopPending） | 停止标志 + kick；返回 restricted 前与阻塞返回前两个安全点停驻至 SIGCONT |
| 调度策略/nice/affinity | `THREAD_SET_PRIORITY`（nice→权重等级）、`THREAD_SET_AFFINITY`；`getcpu` 用 `CPU_CURRENT` |
| user/system CPU 时间拆分 | 内核按 restricted/normal 模式分别计时（`TaskRuntime`），restricted = user |
| 中断处理（`InterruptHandler`） | 每个中断对象一个服务线程：`interrupt_wait` → handler → `interrupt_ack` |
| 帧分配、DMA、MMIO 映射 | VMO（连续 VMO + BTI pin 取得设备地址）、物理 VMO 映射 MMIO |

P2 linuxd 内存设计（替换旧 `memory` 的帧/页表/COW/swap/回收实现）：

- `MemorySet` = 成员进程句柄 + 私有区根 VMAR + VMA 索引（`FallibleMap<起点, Area>`）。每个 Area 对应 VMAR 中的一个映射，
  拆分（部分 munmap/mprotect）只改索引，VMAR 原生处理子区间。
- Area 后备：私有匿名 / brk / 栈 → 可调整大小的匿名 VMO；共享匿名、memfd、tmpfs、SysV shm → 对象自身的 VMO；
  文件映射 → inode 页缓存 VMO（`MAP_SHARED` 直接映射，`MAP_PRIVATE` 映射 `SNAPSHOT_AT_LEAST_ON_WRITE` 子 VMO）；
  设备映射（DRM/音频缓冲）→ 设备缓冲的 VMO。
- fork：私有区 `create_child(SNAPSHOT)`，共享区映射同一 VMO；`MADV_DONTFORK` 跳过、`MADV_WIPEONFORK` 换新零 VMO。
- 缺页：内核按 VMAR 解决；restricted EXCEPTION 退出时 linuxd 按 VMA 索引分类：`MAP_GROWSDOWN` 扩展、
  权限不符 SEGV_ACCERR、已映射但无法提供页（pager 失败/越过 EOF）SIGBUS、未映射 SEGV_MAPERR。
- user copy：持 mm 读锁校验 VMA 后经 `process_read/write_memory` 复制（得到 EFAULT 而不是服务故障）；
  futex 等原子操作先以 `process_read_memory` 探测并调入页，再在 mm 锁内直接原子访问（服务线程运行在该成员地址空间）。
- 页缓存：每个 inode 一个 VMO。tmpfs/memfd/shm 用普通 VMO 本身作存储；ext4/FUSE 用 pager VMO，linuxd 的 pager 线程
  在 `PAGE_REQUEST` 时从后端读入并 `supply_pages`，回写经 `OP_QUERY_DIRTY`/`OP_WRITEBACK_BEGIN`。read/write 走 `vmo_read/write`。
- 删除：swap、后台回收、内存压力、frame 统计（`/proc/meminfo` 等改由 `SystemBasic` 与 linuxd 自身计数投影）。与未配置 `CONFIG_SWAP` 的 Linux 一致：swapon/swapoff 不注册（ENOSYS），没有 `/proc/swaps`，Swap 字段为 0。
- 页缓存 pager：分派线程 + 按需增长的工作线程池（存储读可能阻塞在 FUSE 守护进程上，单线程 pager 会与守护进程自身的补页互锁）；NOWAIT 读（RWF_NOWAIT、io_uring 非阻塞读）不再支持，走阻塞路径。
- 页缓存补页按 `Inode::readahead_pages` 扩展为预读窗口（FUSE 协商的 `max_readahead`）；已驻留页由内核保持不变。
- 已知性能后续项：user copy 经内核调用；可改为 Starnix 式“直接访问 + 异常恢复”，届时需内核提供异常恢复 PC。

P2 其余实现事实（P4 迁入对应 owner 文档）：

- 驱动 DMA：virtqueue ring 与 xHCI 结构用连续 VMO，其余 DMA 缓冲用普通 VMO；全部以设备 BTI 固定（virtio-mmio 用恒等 BTI，
  PCI function 按 requester ID 经 `iommu-map` 取 stream）。GPU 在编码 ATTACH_BACKING/SUBMIT_3D 时固定 GEM 与命令流缓冲。
- 没有当前 Linux 线程的服务线程（启动线程、pager、deferred、中断线程）阻塞时使用 `ServiceThreadTarget`（停驻在自身线程描述符上），
  不再有“启动期轮询中断”路径。
- group stop / ptrace stop 的完成通知在返回用户态前的安全点转入 `Stopped` 时交付。
- RLIMIT_CPU：存在受限进程时 deferred 线程每秒采样一次（SIGXCPU 按 CPU 秒、hard limit 至多晚一秒 SIGKILL）。
- getrusage 的自愿/非自愿切换数来自内核对承载线程的记录（`TaskRuntime`）。
- `MADV_DONTFORK/DOFORK/WIPEONFORK/KEEPONFORK/DONTDUMP/DODUMP` 与私有匿名 `MAP_GROWSDOWN` 已实现（ABI 矩阵 P4 更新）。
- `/proc/meminfo` 中 AnonPages/Mapped/Shmem/Slab/KernelStack/PageTables 为 0（没有跨进程汇总 owner），MemAvailable = MemFree。

### P3 拆分服务

1. devmgr 与驱动服务（virtio-blk/net/gpu/input/sound/console/rng、PCI、xHCI/USB）——已完成（两架构 clippy 通过）。
2. `ext4d`，`linuxd` 经 FUSE 协议访问——已完成（两架构 clippy 通过）。
3. `netd`（smoltcp），`linuxd` 的 INET socket 转发给它。
4. 设备 UAPI 由 `linuxd` 转发给对应服务。

P3 设计（实现约束）：

- 设备模型 trait（`BlockDevice`、`NetworkDevice`、`DisplayDevice`/`GraphicsDevice`、`InputDevice`、`PcmOutput`、`PortDevice`、
  `EntropySource`、`UsbDevice`）是 devmgr 与 linuxd 之间的契约：devmgr 中驱动实现它们，server 适配层把请求映射到驱动；
  linuxd 中 remote client 实现同一 trait，UAPI 层（tty/drm/input/audio/块设备节点）不变。trait 定义与线协议放在共享协议 crate。
- 传输：每个设备一条 channel。同步请求用 `channel_call`（txid 匹配，调用线程直接阻塞）；设备主动事件（完成、输入、接收帧、
  热插拔）由 devmgr 写入事件 channel，linuxd 的接收线程分派到原有通知路径。大块数据经 VMO：块 I/O 与 PCM 使用连接时建立的共享缓冲，
  GEM/DMA-BUF 以 VMO 句柄传递，由 devmgr 以设备 BTI 固定。
- 平台资源（DTB 设备事实、MMIO/IRQ resource、IOMMU/BTI）随驱动迁到 devmgr；linuxd 不再持有根 resource 的设备权限，只保留串口与电源。
- P3.1 实现事实：共享 crate `service-sync`（可睡眠同步原语）、`device-model`（设备类 trait、注册表、`DeviceBacking`、
  `Activity` 通知）、`device-protocol`（每类一个模块，含 server 适配与远端 client；目录、`EventHub`）。devmgr 拥有 DTB、
  MMIO/IRQ、IOMMU/BTI、全部驱动与开机画面（linuxd 经 `boot` 类推进进度）；中断线程只确认设备并请求完成线程分派。
  root-server 先启动 devmgr（根 resource、DTB、目录写端），再启动 linuxd（目录读端，启动参数作为 command line）。
  linuxd 的 Linux 线程在设备请求期间阻塞在 `channel_call` 中（/proc 状态显示为运行，信号在返回后处理）。
- `ext4d`：拥有 ext4 实现与块设备连接；以 FUSE 线协议在 channel 上服务，linuxd 的 FUSE 客户端增加 channel 传输，页缓存补页走 FUSE READ。
- P3.2 实现事实：`filesystem-protocol`（挂载控制：MOUNT 交出块设备连接，回复 FUSE channel 与挂载控制；`MAX_TRANSFER`
  48 KiB 使每条 FUSE 消息装进一条 channel 消息）。linuxd 的 `ext4` 类型读超级块取 UUID（fsid），经
  `BlockDevice::share_range` 把整个源设备（分区平移到整盘）交给 ext4d，再以块设备型 FUSE 挂载（`fuse/service.rs`：
  `default_permissions`、`allow_other`、卸载同步 DESTROY，请求/回复两个转发线程）访问；ro→rw 经挂载控制。
  root-server 创建控制 channel 并启动 ext4d。ext4d 依 FUSE 请求头的 uid/gid 执行创建属主与 set-GID 继承。
  P4 待办：逐项对照旧本地 ext4 inode 操作与 FUSE 路径的语义差异（属性缓存时效、trusted xattr 列举、创建时清除
  set-GID 等）并写入 ABI 矩阵；ext4 引擎的 host 单元测试为 ext4d 重新托管。
- `netd`：拥有 smoltcp 与网卡连接；INET socket 操作经 channel 转发，就绪事件经事件 channel 回到 linuxd 的 poll/epoll。

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
