# 启动与平台当前架构

## 当前设计

- `bootloader/` 是独立 M-mode RustSBI domain；负责 cold boot、PMP、HSM、TIME、IPI、RFENCE、SRST 与 debug console，并通过 typed handoff 进入 kernel。
- `platform::qemu_virt` 解析 DTB，发布 immutable machine facts，探测 firmware capability，并装配 PLIC、UART、RTC 与 VirtIO MMIO adapter。
- cold boot CPU 完成全局初始化；secondary 由 platform HSM operation 启动。startup stack 与 hardware/logical entry projection 只存在于 RISC-V backend。
- firmware status、DTB opaque 与 machine address 不穿过 platform seam；上层只接收 typed facts、operation error 和通用 device façade。

## RISC-V64 / QEMU virt backend

- 当前 machine 依赖 DTB、SBI、PLIC 与 QEMU `virt` 的 MMIO 拓扑。
- RISC-V hart ID 只在 firmware、DTB 与 backend entry 内使用；进入 generic kernel 前必须映射成 logical `CpuId`。
- SBI mask、Sv39、CSR 与汇编都是 backend mechanism，不是通用 kernel contract。

## Known limits

- 没有其他 platform/backend 实现，也没有真实硬件启动声明。
- 设备发现只覆盖当前 QEMU `virt` 已接入的 modern VirtIO 路径。
