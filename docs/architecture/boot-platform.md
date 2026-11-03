# 启动与平台当前架构

## 内核启动

1. `arch` 建立高半映射和本地执行状态，`execution::entry` 把 raw handoff 解码为 `BootContext`。
2. `main` 初始化堆、platform、logical CPU topology、ASID、barrier、frame allocator、VM 与线程；DTB、initrd 和崩溃记录区先从可分配 RAM 中排除。
3. `bootstrap` 从 bootfs 载入 component-manager，交付初始 resource、DTB 与启动参数；发布初始化完成后启动其他 CPU，所有 CPU online 后进入 native 调度器。

内核不挂载根文件系统、不装配通用设备驱动、不启动 Linux init。`boot::cmdline` 只解释日志与 panic 参数。
`platform` 只拥有 machine facts、firmware、中断控制器、早期串口与跨 CPU 同步。

## 系统服务启动

component-manager 启动 driver-manager、fshost、netstack、dhcp-client、netcfg、audio-core 与 linuxd，按设备类派生设备目录并为每个
消费者服务路由目录（见[服务拓扑与能力路由](../architecture-contract/dependency-owners.md#服务拓扑与能力路由)）。
driver-manager 获取设备根 resource、DTB、设备目录服务端以及驱动宿主的 Job 与映像；fshost 获取 ext4 实例的 Job 与映像；
linuxd 获取路由给它的设备类、服务路由目录、串口与电源能力，不持有设备根 resource。
设备发现、PCI 枚举与节点匹配由 driver-manager 执行；VirtIO/xHCI 装配、IRQ 完成与启动画面（首个 GPU 的宿主）由各驱动宿主执行。
linuxd 在自身服务线程中建立 VFS、挂载根及 devtmpfs、选择 console，然后在 restricted mode 中执行 Linux init。
启动参数中的 root/init/console 与 init argv/environment 规则属于 linuxd，见[用户态与 ABI](userspace-abi.md)。

## AArch64 / QEMU virt

- QEMU 按 arm64 Image boot protocol 加载 kernel；x0 交付 DTB，低物理 stub 从 EL2 收敛到 EL1，建立永久 TTBR1 kernel root。没有独立 ARM bootloader。
- kernel platform 从 DTB 验证 enabled CPU、RAM、PL011、PL031、GICv3 与 PSCI HVC；设备 MMIO、PCI window/INTx 和 IOMMU 设备事实由 driver-manager 自行解码。
- GICv3 使用 Group-1、CNTV timer PPI 与 software SGI；PSCI `CPU_ON` 启动 secondary。PL011 hardirq 先清 RX/timeout 中断再 drain FIFO，字节送入 `serial`。
- MMIO 由共用 `mmio::MmioBus` 验证范围/对齐；AArch64 backend 固定 base-register 单次 load/store，避免 HVF 无法解码的 post-index 访问。
- supported baseline 为 ARMv8-A；privileged user-address 隔离使用统一 TTBR0 切换，不依赖 PAN 扩展。执行上下文约束见[执行域](execution.md)。

## RISC-V64 / QEMU virt

- 独立 `bootloader/` 提供 M-mode RustSBI、PMP、HSM、TIME、IPI、RFENCE、SRST 与 debug console，以 DTB 和 typed handoff 进入内核。
- platform 验证 DTB、SBI、PLIC、UART 与 RTC；hart ID 只在 firmware/arch/platform 边界使用，领域只使用 logical `CpuId`。
- RFENCE 通过每 hart 的 request/range/ack mailbox 与串行 sender 发布 SBI `[start,size)`；目标完成实际 fence 后才确认。Sv39、CSR、SBI mask 不进入通用机制。

## 范围

只声明 QEMU virt machine；没有真实硬件、ACPI、EL2 guest、ITS/MSI、PCI bridge/hotplug 的支持声明。
设备 capability 与 IOMMU 限制见[设备与终端](devices-terminal.md)。双架构 DTB 测试以当前 QEMU 的实际 `dumpdtb` 为输入，不能依赖可变节点名或猜测地址。
