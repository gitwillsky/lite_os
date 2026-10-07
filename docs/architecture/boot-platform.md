# 启动与平台当前架构

## 当前设计

- `platform::qemu_virt::{aarch64,riscv64}` 是同一 machine family 的编译期 backend；共同 seam 只发布 immutable machine facts、CPU identity、firmware operation、interrupt token 与通用设备 façade。
- cold boot CPU 完成全局初始化；secondary 只通过所选 platform operation 启动。raw hardware identity 在进入 generic CPU topology 前完成 logical `CpuId` 投影。
- firmware status、DTB opaque 与 machine address 不穿过 platform seam；上层只接收 typed facts、operation error 和通用 device façade。

## AArch64 / QEMU virt backend

- QEMU 直接按 Linux arm64 Image protocol 加载 release kernel；x0 是唯一 DTB handoff，低物理 boot stub 从 EL2 收敛到 EL1 后建立高半内核映射，不存在 ARM bootloader 或兼容启动入口。
- platform 严格要求 DTB 中的 enabled CPU、`dma-coherent`、PL011、PL031、GICv3、PSCI HVC 与 modern VirtIO MMIO；UTM 产品拓扑还要求 `pci-host-ecam-generic` 的 ECAM、32-bit MMIO window 与 INTx route。缺失必需事实时 fail-stop，不猜测 QEMU 默认地址。
- PL011 RX 在 unmask 前启用 16-byte FIFO 与最低接收阈值；hardirq 每次有界 drain 全部当前可读
  bytes。这样 host stdio 无硬件流控时的批量输入不会退化到 reset 后的 1-byte holding register。
- `arch::io` 用内联静态 façade 固定 AArch64 MMIO 为 base-only `LDRB/STRB/LDR/STR`，通用
  `MmioBus` 保留边界/对齐 owner。该形态既阻止 VirtIO input config loop 被优化成 HVF 无法
  解码的 post-index access，也不增加调用、锁、分配或运行时架构分派。
- GICv3 只启用 Group-1 GICD/GICR/ICC、timer PPI 27 与单一 software SGI；PSCI
  `CPU_ON` 启动 secondary。PCI 只实现 ECAM enumeration、memory BAR assignment 与
  legacy INTx，供 UTM 原生 VirtIO Console/SPICE agent 使用；ITS/MSI、PCI
  hotplug/bridge、secure world、EL2 guest、ACPI 均不在当前产品范围。

## RISC-V64 / QEMU virt backend

- `bootloader/` 是独立 M-mode RustSBI domain；负责 cold boot、PMP、HSM、TIME、IPI、RFENCE、SRST 与 debug console，并通过 typed handoff 进入 kernel。
- 当前 machine 依赖 DTB、SBI、PLIC、UART、RTC 与 QEMU `virt` 的 MMIO 拓扑。
- RISC-V hart ID 只在 firmware、DTB 与 backend entry 内使用；进入 generic kernel 前必须映射成 logical `CpuId`。
- SBI mask、Sv39、CSR 与汇编都是 backend mechanism，不是通用 kernel contract。
- RFENCE 使用每 hart 单槽 request/range/ack mailbox；全局 sender lock 串行发布，目标 hart 按 SBI `[start,size)` 逐页 fence 后 ack。whole-address-space 只使用规范定义的两个 sentinel。

- kernel command line 来自 DTB `/chosen/bootargs`（platform 复制保存，`/proc/cmdline` 原样回显）。
  `cmdline::parse` 按 Linux `next_arg` 分词与引号规则取出内核参数：`init=`、`root=`（`/dev/<disk>` 或
  十进制 `MAJ:MIN`）、`rootfstype=`（只接受 ext4）、`ro`/`rw`、`rootwait`、`console=name[,options]`（最后
  一个生效）、`loglevel=`/`quiet`/`debug`；其余按 Linux `unknown_bootoption` 转交 init：`name=value` 进
  环境（以 `HOME=/`、`TERM=linux` 开始，同名覆盖），裸词与 `--` 之后的词进 argv，带 `.` 的模块参数忽略，
  argv/环境各最多 32 项。指定 `init=` 时只尝试它，失败即 panic；否则依次尝试 `/sbin/init`、`/etc/init`、
  `/bin/init`、`/bin/sh`。
- 启动顺序由零大小证明 token 在类型上约束，token 只能由完成该步骤的 owner 构造：
  `fs::init_vfs` → `VfsReady` → `task::initialize(VfsReady)` → `SchedulerReady`；内核线程能力只能由
  `task::kernel_thread_support(SchedulerReady)` 取得，因此 `MountEnvironment` 与 `fs::mount_root`
  （选择根文件系统类型、创建写回内核线程、挂载 devtmpfs）只能在调度器就绪后发生，并返回
  `RootMounted`；`tty::init` 返回 `ConsoleReady`；`task::spawn_init` 同时要求 `SchedulerReady`、
  `RootMounted` 与 `ConsoleReady`。顺序错误无法编译。composition root 不依赖具体文件系统类型或
  console adapter。
- console 设备由 platform 以 Linux 设备名（aarch64 `ttyAMA0`、riscv64 `ttyS0`）与同步单字节输出原语
  经 `drivers::console::register_serial` 发布；composition root 按 `console=` 名称从 drivers 注册表
  选择，fs TTY 把它包装为系统 console Terminal。

## Known limits

- `root=` 支持 `/dev/<name>`、`MAJ:MIN`、`PARTUUID=`、`UUID=`、`LABEL=`；后三者扫描已发布块设备（`PARTUUID` 取自
  GPT 项 GUID 或 MBR `签名-分区号`，`UUID`/`LABEL` 读 ext4 超级块），同一标识匹配多个设备取发布顺序第一个；
  不支持 `PARTUUID=…/PARTNROFF=`。`/proc/mounts` 的 source 是解析出的 `/dev/<name>`。缺省时以首块盘为根。
  `ro` 需要 remount 才能转为可写，尚未支持，启动明确失败。`console=` 在已注册 console 设备中按名称
  选择，选项忽略；指向不存在的 console 时启动明确失败，而不是像 Linux 那样在没有 `/dev/console` 的
  情况下运行 init。`loglevel=` 映射到全局 severity threshold，
  同时作用于 kmsg ring，N ≤ 3 按只输出 Error 处理。

- 没有 QEMU `virt` 之外的 machine backend，也没有真实硬件启动声明。
- 设备发现只覆盖当前 QEMU `virt` 已接入的 modern VirtIO 路径。DTB 解码由 verify-unit 以当前 host QEMU
  `dumpdtb` 输出测试；QEMU 升级改变 DTB 布局时在 unit 阶段失败。
