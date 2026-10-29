# 领域依赖说明

## `arch`

编译期选择的 ISA mechanism；page-table frame owners 使用 fallible ordered storage 保持精确 physical identity，不消费 platform 或上层领域状态

## `entry`

raw boot/trap callback ABI 的唯一 codec；boot 只构造 typed `BootContext`，trap 只投递 generic semantic handler

## `config`

只保存无运行时依赖的常量

## `crash`

fail-stop 诊断策略；通过静态 arch/MMU/context façade、安全 log 快照与 typed display seam 采集现场。
boot 装配保留区，memory 不反向依赖本领域；procfs 只经已有 source adapter 读取不可变摘要。
IPI 停止检查位于 ack 与 memory-barrier completion 之后；watchdog 由 trap tick 检查、task deferred 消费报告进展。

## `cpu`

logical `CpuId`/`CpuSet`、hardware identity 映射与 online/active lifecycle 的唯一 owner；不感知 platform，hardware identity 只在 platform/firmware 边界出现

## `interrupt`

位于 `execution::interrupt` 的设备中断处理契约，由平台中断控制器分发、驱动实现；ISA 中断等待与屏障仍由 `arch` 提供。

## `console`

串口 console 设备与唯一 RX 队列；平台 UART handler 只发布已 drain 的字节，TTY 绑定该设备并拥有终端语义。不依赖文件系统或 DMA 内存。

## `device_registry`

设备注册容器提供按发现顺序编号的只追加发布；设备类注册表与 console 各自拥有唯一实例，容器不拥有硬件或 UAPI。

## `deferred`

per-CPU merged deferred-work bitmap 与设备类 handler 注册表的唯一 owner；空→非空 transition 经 `arch::interrupt::raise_software` 发布 local edge，发布者不认识消费者；独立于 `execution::cpu` 以免 CPU topology 依赖 platform

## `platform`

编译期选择的 machine/firmware adapter；拥有 DTB、PSCI/SBI、GIC/PLIC、UART 与 TLB shootdown，并向总线驱动暴露 transport 表、PCI host、地址映射与 interrupt 注册；只实现早期 console 与中断控制契约，不消费设备类 DMA/UAPI，也不依赖 `memory`

## `fallible_tree`

无状态的确定性 AVL mechanism；提供显式 OOM publication、结构化 split 与 ordered-disjoint join，不拥有领域数据

## `sync`

锁与 IRQ transfer 只依赖本地中断 mechanism 和 logical `CpuId`；transfer token 在错误 CPU restore 时 fail-stop，禁止把 hardware identity 引入同步领域

## `memory`

VMA/frame policy；页表只通过 `arch::mmu` 的静态 frame-owner adapter，不感知具体 ISA encoding；初始栈只接收 exec 准备的 strings/AT_RANDOM，不访问 entropy device

## `boot_screen`

内核开机画面的唯一 owner：解码共用 crate `boot-scene` 的 `LBS1` 位图，整数缩放居中后画进 boot scanout，并按启动里程碑推进进度条；只经 `devices::model::BootFramebuffer` 触达显示，不认识具体 adapter

画面纯属装饰：任何绘制失败只记录警告并停止更新，不得阻止启动。DRM 绑定 completion work 之后 adapter 拒绝同步刷新，进度条改由 compositor 从 `KERNEL_PROGRESS_PERMILLE` 接着画

## `drivers`

按总线组织硬件实现，入口只声明子模块；设备模型在 `devices::model`；各总线分别受依赖围栏约束。ALSA、DRM、evdev 等用户态 ABI 领域仍独立，通过设备接口调用 adapter。

## `pci`

PCI ECAM 根总线枚举、配置空间与唯一 BAR 资源分配；消费 platform 的 host 窗口与中断路由事实，由 VirtIO PCI 使用；后续 USB host adapter 复用同一资源 owner。具体设备协议不属于 PCI owner。

## `usb`

`drivers::usb` 拥有 xHCI command/event/transfer rings、USB root-port attachment 与 UAC2 playback adapter。只消费 PCI inventory、platform 地址/INTx、DMA backing 与 PCM seam；不认识 ALSA 文件、task TCB 或宿主音频设备。
`main` 创建枚举线程，task-only control 事务与 deferred packet completion 分离。

## `device`

位于 `devices::model` 的设备类 seam（display、graphics、input、network、PCM output、entropy、named port）、只追加的设备注册表与 driver I/O completion；块 seam 属于 `block`，总线 adapter 属于 `drivers::virtio` 等具体总线领域。
MMIO 属于 `arch::mmio`，中断契约与 console 分别由 `interrupt`、`console` 拥有

## `block`

块设备 trait 与只追加注册表、分区表（MBR/EBR/GPT）解析、分区视图、字节范围 I/O 与 `PARTUUID` 文本；驱动实现 trait，`fs` 只消费；分区表与范围 I/O 是可在 host 上单测的纯函数

## `virtio`

VirtIO transport（MMIO/PCI）、virtqueue/DMA、各设备类 adapter 与设备装配表 `binding`；只实现设备模型与块层接口，消费 platform 的发现事实；MMIO 与 ISA 机制仅调用静态 arch façade

## `virtio_port`

拥有唯一 named byte-stream 的 task readiness、blocking/poll projection，并经 fs 字符设备注册表发布 `virtio-ports/*` 节点；不解析 SPICE message 或感知 platform/QEMU

## `audio`

拥有 Linux ALSA playback 子流、混音、Master/PCM control、position、poll/xrun、PCM/control 设备文件及其 ioctl UAPI；
只经通用 PCM output seam 使用 device adapter。composition root 提供实际调用 TID 的 namespace identity observer，
audio 不直接访问 task；参考 timestamp 只消费 timer 的公共时钟投影。

## `drm`

只消费通用 display seam；拥有 `dri/card0` 设备文件与 DRM/virtio-gpu ioctl UAPI；GEM handle 用 fallible ordered publication；mode 变化只经 socket 发布 uevent；不感知 VirtIO adapter 或 task

## `input`

只消费通用 input seam，并拥有 evdev 事件域、`input/eventN` 设备文件与 evdev ioctl UAPI；不感知 VirtIO adapter 或 task

## `io_uring`

拥有 io_uring ring 的共享内存布局、SQE 校验、CQE 发布与溢出顺序，以及 ring fd 的 poll/mmap 文件操作；经 `fs::chardev::DeviceFile` 发布并以 `Any` 取回 ring，`fs` 不反向依赖；不感知 fd 表、task 或 syscall，fd 查询经 `FdExists` 注入

## `ipc`

Pipe byte/endpoint、eventfd 与实际 IPC namespace/System V message queue、semaphore、shared-memory segment、atomic operation 与 SEM_UNDO membership 的对象、权限、IDs/key 和 waiter owner；`memory` 仅供实际共享页 backing，不反向导入 IPC；不感知 fd、task
、socket 或 syscall，namespace 只读 identity seam 不反向依赖 IPC；`id` 分配 anonymous inode及有序领域publication identity

## `net`

拥有 socket domain facade、AF_UNIX 与 AF_INET stack；只消费 `devices::model` 的 network-device 接口，`id` 分配 anonymous inode及有序领域publication identity

## `fs`

VFS、具体文件系统、字符设备注册表、inotify 与 mem/kmsg 设备（kmsg 经 `execution::deferred` 唤醒）；OFD 归 `file`、终端归 `devices::tty`，advisory lock 经 `LockHolder` 识别持锁者。
FUSE ring 只经 `execution::cpu` 的 logical CpuId/possible/current_id 为标准 qid 路由，不读取 hardware identity 或 arch adapter。
procfs 经 `net` 的 NET identity 和只读快照投影 networking 事实，不持有另一份协议/端口/路由状态。

FUSE 后备文件只通过 `BackingFile`/`BackingOpen` 与 user-copy seam 注入，fd/凭据捕获属于 syscall adapter；
映射机制沿已有 memory seam 返回，不反向依赖 file 或 task。

## `file`

open file description、fd table、epoll/timerfd/readiness 与 inode/设备/pipe/socket/eventfd 后端的统一投影；fd 层位于 `fs` 与 `net` 之上，汇聚完整文件后端生命周期。
`memory::MappedFileOwner` 只标记实际 OFD 的 mapping pin，不保存另一份 file/mm 状态，也不引入 memory 到 file 的反向依赖。

## `tty`

Terminal line discipline、`/dev/tty`/`console`/`ptmx`、Unix98 pty 与 devpts 文件系统；session、process group 与 signal 归 task，经 `install_job_control` 注入；经 `fs::chardev`/`fs::devnode` 注册设备文件

## `namespace`

User namespace 的 immutable ancestry、ID maps 与 setgroups publication；UTS name owner、Time namespace offset/freeze owner、PID namespace 的 local-number/reaper/lifecycle owner；namespace reference 只保活 actual NamespaceOwner 的只读 identity，typed Arc validation 不
分配；具体 domain 内容不反向导入；不查 Task、不读 user memory

## `task`

调度用 logical CPU；graph 串行提交 seccomp/TSYNC、clone policy 与退出；exec 获取 AT_RANDOM；deferred 经注册 handler 投递

## `trap`

只处理 `arch::trap::TrapEvent`、领域投递和用户返回 orchestration，不读取 CSR

## `syscall`

只编解码通用 UAPI、user-copy 游标与 errno；设备专属 UAPI 经 `fs::chardev::DeviceFile` 由设备子系统拥有；只经 `sync::WaitResult` 消费阻塞结果

## `random`

entropy facade；只消费 RNG device seam，不生成伪随机 fallback

## `system`

whole-system policy；ISA 用户事实只经 `arch::user`，CPU/firmware 只经各自 facade

## `timer`

RTC 与 per-CPU deadline 由 timer 唯一拥有

## `log`

日志策略、有界 record owner 与输出在本 module 内闭合

## `id`

纯 ID allocation mechanism

## `cmdline`

纯 kernel command line 解析：取出内核参数，其余按 Linux 规则转交 init

## `lang_item`

只使用 typed diagnostic identity 与 architecture/platform fail-stop mechanism

## `main`

装配

## Crate 与目录装配


- `bootloader` 是独立 firmware domain，不依赖 kernel 或 userspace。
- `boot-scene` 是供内核和后续用户态共用的 `no_std` crate，唯一实现 `LBS1` 位图的解析校验、逐行解码、整数缩放居中与进度矩形，不依赖其他实现 crate。
- 内核编入的启动画面由构建环境变量 `LITEOS_BOOT_SCENE` 选择，选择逻辑只在 `scripts/classic_assets.py`。
- `syscall-abi` 只保存 dispatcher 接入的 Linux 64-bit asm-generic 编号与明确的 architecture extension，不依赖实现 crate。
- `kernel/src/main.rs` 是唯一 composition root；初始化顺序和 adapter 装配不得下沉到 driver、filesystem 或 task。
- binary crate 的跨 module interface 默认使用最窄的 `pub(super)`、`pub(in …)` 或 `pub(crate)`；扩大范围必须说明 production caller。

内核按领域组织，目录与 Rust module 路径一致；`boot`、`execution`、`devices`、`drivers` 入口只声明子领域，不拥有运行时状态。每个子领域仍有独立的正向依赖契约，具体映射由 architecture-check 的 `module_layout` 维护。

- `arch` 拥有 ISA/MMU 与 MMIO mechanism，`platform` 提供 machine/firmware 资源事实。
- `devices::model` 提供设备类接口，`devices::block` 拥有块语义，ALSA/DRM/evdev/TTY 各自拥有设备 UAPI。
- `drivers` 按实际总线与硬件协议组织实现；设备发现后的绑定顺序由 `main` 装配。
- `execution` 汇集 CPU、任务、同步、中断与 trap，`boot` 汇集启动参数与启动画面；文件系统、网络、内存等保留独立领域。

子模块可使用 `<name>.rs` 加 `<name>/`，也可使用 `<name>/mod.rs`，遵循 Rust 标准布局，不为行数或测试夹具机械拆目录。

host 夹具把内核文件编译进测试 crate 时，rustc 把被包含的文件当作 mod-rs 文件，到它所在目录找子模块，与标准布局不同。
内核布局不为此让步：`tools/kernel-view/` 为被夹具包含且带子模块的内核文件生成只含符号链接的“平铺视图”，
夹具改为包含视图里的文件；`scripts/kernel_fixture_view.py --write` 生成，`make verify-unit` 校验。

DRM PRIME 的共享资源、context attachment 与退休均由 drm 拥有；syscall 只经 DeviceFile/UserMemory 发布 FD，
不编解码 DRM 结构。DeviceFile 的可选弱 OFD identity 允许重复导出复用真实 open file description，不反向强引用。
fs::GraphicsSysfsSource 只接收 DRM 装配的只读实际设备 identity；sysfs 不依赖 graphics driver 或解析 VirtIO 寄存器。
