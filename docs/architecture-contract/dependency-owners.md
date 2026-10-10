# 领域依赖说明

## `arch`

编译期选择的 ISA mechanism；page-table frame owners 使用 fallible ordered storage 保持精确 physical identity，不消费 platform 或上层领域状态

## `entry`

raw boot/trap callback ABI 的唯一 codec；boot 只构造 typed `BootContext`，trap 只投递 generic semantic handler

## `config`

只保存无运行时依赖的常量

## `cpu`

logical `CpuId`/`CpuSet`、hardware identity 映射与 online/active lifecycle 的唯一 owner；不感知 platform，hardware identity 只在 platform/firmware 边界出现

## `hal`

硬件抽象与 platform/driver 共用的最小 seam；唯一的设备寄存器访问路径 `MmioBus`（platform 与 virtio 共用）、设备中断接口、串口 console seam 与只追加设备注册表；位于 `platform` 与 `drivers` 之下，platform 不依赖设备类或 DMA 内存

## `deferred`

per-CPU merged deferred-work bitmap 与设备类 handler 注册表的唯一 owner；空→非空 transition 经 `arch::interrupt::raise_software` 发布 local edge，发布者不认识消费者；独立于 `cpu` 以免 CPU topology 依赖 platform

## `platform`

编译期选择的 machine/firmware adapter；拥有 DTB、PSCI/SBI、GIC/PLIC、UART 与 TLB shootdown，并向 `virtio` 暴露 transport 表、PCI host、地址映射与 interrupt 注册；不认识设备类，也不依赖 `memory`

## `fallible_tree`

无状态的确定性 AVL mechanism；提供显式 OOM publication、结构化 split 与 ordered-disjoint join，不拥有领域数据

## `sync`

锁与 IRQ transfer 只依赖本地中断 mechanism 和 logical `CpuId`；transfer token 在错误 CPU restore 时 fail-stop，禁止把 hardware identity 引入同步领域

## `memory`

VMA/frame policy；页表只通过 `arch::mmu` 的静态 frame-owner adapter，不感知具体 ISA encoding；初始栈只接收 exec 准备的 strings/AT_RANDOM，不访问 entropy device

## `drivers`

设备类 seam（display、graphics、input、network、PCM output、entropy、named port）、只追加的设备注册表与 driver I/O completion；块 seam 属于 `block`，总线 adapter 属于 `virtio`，MMIO/中断/console 属于 `hal`

## `block`

块设备 trait 与只追加注册表、分区表（MBR/EBR/GPT）解析、分区视图、字节范围 I/O 与 `PARTUUID` 文本；驱动实现 trait，`fs` 只消费；分区表与范围 I/O 是可在 host 上单测的纯函数

## `virtio`

VirtIO transport（MMIO/PCI）、virtqueue/DMA、各设备类 adapter 与设备装配表 `binding`；只实现 `drivers`/`block` 的 seam，消费 platform 的设备发现事实，不依赖 `arch`

## `virtio_port`

拥有唯一 named byte-stream 的 task readiness、blocking/poll projection，并经 fs 字符设备注册表发布 `virtio-ports/*` 节点；不解析 SPICE message 或感知 platform/QEMU

## `audio`

拥有 Linux ALSA PCM playback 领域状态、position、poll/xrun、`snd/pcmC0D0p` 设备文件与 PCM ioctl UAPI；只经通用 PCM output seam 使用 device adapter

## `drm`

只消费通用 display seam；拥有 `dri/card0` 设备文件与 DRM/virtio-gpu ioctl UAPI；GEM handle 用 fallible ordered publication；mode 变化只经 socket 发布 uevent；不感知 VirtIO adapter 或 task

## `input`

只消费通用 input seam，并拥有 evdev 事件域、`input/eventN` 设备文件与 evdev ioctl UAPI；不感知 VirtIO adapter 或 task

## `ipc`

Pipe byte/endpoint、eventfd 与实际 IPC namespace/System V message queue、semaphore、shared-memory segment、atomic operation 与 SEM_UNDO membership 的对象、权限、IDs/key 和 waiter owner；`memory` 仅供实际共享页 backing，不反向导入 IPC；不感知 fd、task
、socket 或 syscall，namespace 只读 identity seam 不反向依赖 IPC；`id` 分配 anonymous inode及有序领域publication identity

## `socket`

拥有 socket domain facade、AF_UNIX 与 AF_INET stack；`drivers` 只允许 network-device seam，`id` 分配 anonymous inode及有序领域publication identity

## `fs`

VFS、具体文件系统、字符设备注册表、inotify 与 mem/kmsg 设备（kmsg 经 `deferred` 唤醒）；OFD 归 `file`、终端归 `tty`，advisory lock 经 `LockHolder` 识别持锁者。
procfs 经 `socket` 的 NET identity 和只读快照投影 networking 事实，不持有另一份协议/端口/路由状态。

## `file`

open file description、fd table、epoll/timerfd/readiness 与 inode/设备/pipe/socket/eventfd 后端的统一投影；fd 层位于 `fs` 与 `socket` 之上，汇聚完整文件后端生命周期。
`memory::MappedFileOwner` 只标记实际 OFD 的 mapping pin，不保存另一份 file/mm 状态，也不引入 memory 到 file 的反向依赖。

## `tty`

Terminal line discipline、`/dev/tty`/`console`/`ptmx`、Unix98 pty 与 devpts 文件系统；session、process group 与 signal 归 task，经 `install_job_control` 注入；经 `fs::device` 注册设备文件

## `namespace`

User namespace 的 immutable ancestry、ID maps 与 setgroups publication；UTS name owner、Time namespace offset/freeze owner、PID namespace 的 local-number/reaper/lifecycle owner；namespace reference 只保活 actual NamespaceOwner 的只读 identity，typed Arc validation 不
分配；具体 domain 内容不反向导入；不查 Task、不读 user memory

## `task`

调度用 logical CPU；graph 串行提交 seccomp/TSYNC、clone policy 与退出；exec 获取 AT_RANDOM；deferred 经注册 handler 投递

## `trap`

只处理 `arch::trap::TrapEvent`、领域投递和用户返回 orchestration，不读取 CSR

## `syscall`

只编解码通用 UAPI、user-copy 游标与 errno；设备专属 UAPI 经 `fs::device::DeviceFile` 由设备子系统拥有；只经 `sync::WaitResult` 消费阻塞结果

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
- `syscall-abi` 只保存 dispatcher 接入的 Linux 64-bit asm-generic 编号与明确的 architecture extension，不依赖实现 crate。
- `kernel/src/main.rs` 是唯一 composition root；初始化顺序和 adapter 装配不得下沉到 driver、filesystem 或 task。
- binary crate 的跨 module interface 默认使用最窄的 `pub(super)`、`pub(in …)` 或 `pub(crate)`；扩大范围必须说明 production caller。


源码目录按领域分组，module 树保持扁平：
- `kernel/src/hardware/`：`arch`、`platform`、`hal`、`cpu`、`deferred`、`entry`。
- `kernel/src/storage/`：`block`、`fs`。
- `kernel/src/devices/`：`drivers`、`virtio`、`virtio_port`、`audio`、`drm`、`input`、`tty`。
- 其余 module 直接位于 `kernel/src/`。分组目录经 `main.rs` 的 `#[path]` 声明，因此 module 路径（`crate::fs`）和下表 owner 名不含分组。
- 领域 module 的根文件是 `<module>/mod.rs`；子文件用 `<name>.rs` 加同名目录，并显式 `#[path]` 声明。
