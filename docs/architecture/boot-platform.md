# 启动与平台当前架构

## 当前设计

- `platform::qemu_virt::{aarch64,riscv64}` 是同一 machine family 的编译期 backend；共同 seam 只发布 immutable machine facts、CPU identity、firmware operation、interrupt token 与通用设备 façade。
- cold boot CPU 完成全局初始化；secondary 只通过所选 platform operation 启动。raw hardware identity 在进入 generic CPU topology 前完成 logical `CpuId` 投影。
- firmware status、DTB opaque 与 machine address 不穿过 platform seam；上层只接收 typed facts、operation error 和通用 device façade。

## AArch64 / QEMU virt backend

- QEMU 直接按 Linux arm64 Image protocol 加载 release kernel；x0 是唯一 DTB handoff，低物理 boot stub 从 EL2 收敛到 EL1 后建立高半内核映射，不存在 ARM bootloader 或兼容启动入口。
- platform 严格要求 DTB 中的 enabled CPU、`dma-coherent`、PL011、PL031、GICv3、PSCI HVC 与 modern VirtIO MMIO；缺失必需事实时 fail-stop，不猜测 QEMU 默认地址。
- PL011 RX 在 unmask 前启用 16-byte FIFO 与最低接收阈值；hardirq 每次有界 drain 全部当前可读
  bytes。这样 host stdio 无硬件流控时的批量输入不会退化到 reset 后的 1-byte holding register。
- `arch::io` 用内联静态 façade 固定 AArch64 MMIO 为 base-only `LDRB/STRB/LDR/STR`，通用
  `MmioBus` 保留边界/对齐 owner。该形态既阻止 VirtIO input config loop 被优化成 HVF 无法
  解码的 post-index access，也不增加调用、锁、分配或运行时架构分派。
- GICv3 只启用 Group-1 GICD/GICR/ICC、timer PPI 27 与单一 software SGI；PSCI `CPU_ON` 启动 secondary。ITS/MSI/PCI、secure world、EL2 guest、ACPI 均不在当前产品范围。

## RISC-V64 / QEMU virt backend

- `bootloader/` 是独立 M-mode RustSBI domain；负责 cold boot、PMP、HSM、TIME、IPI、RFENCE、SRST 与 debug console，并通过 typed handoff 进入 kernel。
- 当前 machine 依赖 DTB、SBI、PLIC、UART、RTC 与 QEMU `virt` 的 MMIO 拓扑。
- RISC-V hart ID 只在 firmware、DTB 与 backend entry 内使用；进入 generic kernel 前必须映射成 logical `CpuId`。
- SBI mask、Sv39、CSR 与汇编都是 backend mechanism，不是通用 kernel contract。
- RFENCE 使用每 hart 单槽 request/range/ack mailbox；全局 sender lock 串行发布，目标 hart 按 SBI `[start,size)` 逐页 fence 后 ack。whole-address-space 只使用规范定义的两个 sentinel。

## Known limits

- 没有 QEMU `virt` 之外的 machine backend，也没有真实硬件启动声明。
- 设备发现只覆盖当前 QEMU `virt` 已接入的 modern VirtIO 路径。
