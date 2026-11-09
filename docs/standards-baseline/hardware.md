# 硬件规范基线

本页维护外设和消息中断的固定规范与一手来源；Linux revision 与裁决顺序沿用[总基线](../standards-baseline.md)。

## USB host 与 USB Audio

- xHCI Requirements Specification `1.2`，May 2019；[Intel 原文](https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf)，SHA-256 `9257ca24f34bff08e6020623f707c788ca0f0df657aa713309ff489a68b1731e`。
- USB 2.0，April 27, 2000；[USB-IF 发布入口](https://www.usb.org/document-library/usb-20-specification)。root-port debounce/reset recovery、standard requests 与 high-speed isochronous/feedback 以该规范为准。
- USB Device Class Definition for Audio Devices `2.0`，errata/ECN through September 14, 2026；[USB-IF 原文](https://www.usb.org/sites/default/files/Audio2_with_Errata_and_ECN_through_Sep_14_2026.pdf)。Type-I PCM 以同发布组的 Audio Data Formats 2.0 为准。
- clock/format/feedback 的成熟实现参照[固定 Linux revision](../standards-baseline.md#linux-arm64--riscv64-abi) 的 `sound/usb/clock.c`、`format.c`、`endpoint.c`；xHCI ring/stop/retirement 参照 `drivers/usb/host/xhci-ring.c`。

## VirtIO

- Virtual I/O Device `1.4` Committee Specification 01；source commit `917e900e0246b7fe21cdde795b0e566dd4f57d8d`。
- 来源：[CS01 HTML](https://docs.oasis-open.org/virtio/virtio/v1.4/cs01/virtio-v1.4-cs01.html)、[固定源码](https://github.com/oasis-tcs/virtio-spec/tree/917e900e0246b7fe21cdde795b0e566dd4f57d8d)。
- 定义 transport、feature、virtqueue、DMA 与 reset；TX 校验和采用 [RFC 1071](https://www.rfc-editor.org/rfc/rfc1071.html) 的并行求和与奇偶规则，不使用其增量更新公式。
- 标准 PCI capability、MSI/MSI-X 的寄存器布局参照[固定 Linux revision](../standards-baseline.md#linux-arm64--riscv64-abi) 的 `include/uapi/linux/pci_regs.h`，链遍历语义参照 `drivers/pci/access.c`；具体支持范围由设备契约维护。

## Arm PrimeCell UART

- PL011 Technical Reference Manual `DDI 0183G`，PrimeCell UART revision `r1p4`。
- 来源：[Arm 固定手册](https://documentation-service.arm.com/static/5e8e3655fd977155116a9042)。
- 它定义 UART register layout、reset state、FIFO 与 interrupt threshold；AArch64 QEMU `virt`
  backend 不从 QEMU 行为反推这些 machine facts。

## 消息中断与 GIC

- Arm GICv3/v4 Architecture Specification `IHI 0069H.b`、`ID041224`（2024），
  [Arm 固定 PDF](https://documentation-service.arm.com/static/67eaa4d098aa3c3b6eea7351)。
  ITS/LPI 表、MAPD/MAPC/MAPTI/MOVI/INV/CLEAR/DISCARD/SYNC 与完成语义以第 5 章和寄存器定义为准。
- 成熟实现参照总基线固定 Linux `drivers/irqchip/irq-gic-v3-its.c`、`include/linux/irqchip/arm-gic-v3.h`、
  `drivers/of/base.c::of_map_id` 与 `drivers/virtio/virtio_pci_modern.c`（先安装 queue vector 再 enable）；
  组分配保活子 Interrupt 的对象寿命参照 Fuchsia 固定 revision `be87bce72d45` 的
  [MSI 对象文档](https://fuchsia.googlesource.com/fuchsia/+/be87bce72d45/docs/reference/kernel_objects/msi.md)。
  LiteOS 使用自己的 native capability/BTI 装配，不声明 Zircon syscall 或 PCI 编码兼容。

- MSI-X 源控制与映射寿命参照 Fuchsia revision `003d1cb45d4813494b8223b4165e47af24e6b341` 的
  [MsiInterruptDispatcher](https://fuchsia.googlesource.com/fuchsia/+/003d1cb45d4813494b8223b4165e47af24e6b341/zircon/kernel/object/msi_interrupt_dispatcher.cc)。
  IMSIC 的 CPU 本地向量分配、旧 pending 迁移和同步参照总基线固定 Linux `drivers/irqchip/irq-riscv-imsic-{state,platform}.c`。

## RISC-V Advanced Interrupt Architecture

- DTB 解析使用 crates.io `dtb-walker 0.2.0-alpha.3`，唯一生产源为 `vendor/dtb-walker`；
  原始摘要与改动分别见该目录的 `UPSTREAM.sha256.json`、`LITEOS.md`。
  walker 更新 cell 上下文后也输出原始属性，使上层能校验控制器绑定；不并存另一套 parser。
- Ratified AIA `1.0`，文档版本 `20250312`、commit `20318d1b03f0ac8253458b82f8244d0d0db074e9`；[官方固定发布](https://github.com/riscv/riscv-aia/releases/tag/20250312)。
- IMSIC interrupt file、EIID、MSI 地址/数据与本地 CSR 语义取自[固定 IMSIC 源文档](https://github.com/riscv/riscv-aia/blob/20318d1b03f0ac8253458b82f8244d0d0db074e9/src/IMSIC.adoc)，
  APLIC 的 MSI/level 源语义取自同一发布的 `src/AdvPLIC.adoc`。
- 发现与迁移的成熟实现参照总基线固定 Linux `Documentation/devicetree/bindings/interrupt-controller/riscv,{imsics,aplic}.yaml`
  与 `drivers/irqchip/irq-riscv-{imsic-state,imsic-platform,aplic-msi}.c`；不得用 hardware CPU ID 冒充逻辑 CPU 或 IMSIC 文件排列序号。

- M-mode APLIC MSI 几何与源委托、Smstateen CSR 授权参照 OpenSBI `v1.7` 的
  [aplic.c](https://github.com/riscv-software-src/opensbi/blob/v1.7/lib/utils/irqchip/aplic.c) 与
  [CPU 初始化源码](https://github.com/riscv-software-src/opensbi/tree/v1.7/lib/sbi)；
  LiteOS 保留自己的 RustSBI 固件，仅按固定 AIA 规范实现所需初始化。
