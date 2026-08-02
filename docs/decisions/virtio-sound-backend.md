# ADR: 首期音频硬件只使用 VirtIO Sound

- 状态：已接受
- 日期：2026-07-26

## 背景

LiteOS 的 QEMU `virt` platform 已使用 modern VirtIO-MMIO；AArch64 UTM 路径的 PCI 范围只承载
原生 VirtIO Console/SPICE agent，没有 PCI sound、HDA controller 或 AC97 adapter。本机 QEMU 提供
`virtio-sound-device` 和 CoreAudio/WAV/none host backend。

## 决策

首期唯一音频硬件 backend 是 VirtIO Sound device ID 25，规范固定到
[VirtIO 1.4 基线](../standards-baseline.md#virtio)。QEMU 使用
`virtio-sound-device,streams=1`，只暴露 playback stream：

- 交互图形运行使用 CoreAudio host backend；
- 自动 runtime gate 使用 WAV 或 none backend；
- kernel adapter 独占 controlq、eventq、txq、DMA、PCM command lifecycle、completion、xrun 与 reset；
- `drivers` 只发布通用 PCM output seam，platform 独占 DTB discovery 与 adapter 装配。

不增加 PCI sound、HDA、AC97、USB Audio 或第二种音频 adapter。

## 结果

- AArch64/HVF first-class path 必须覆盖 CoreAudio/WAV runtime、正确性与性能 gate。
- RISC-V 使用同一 generic driver/userspace source 并通过 compile、architecture/static 与 artifact gate，
  但首期不声明声音 runtime 或延迟通过；不得增加 dummy device、silent-success 或 TCG 专用 engine。
- 设备参数、queue 和 VirtIO status 不进入 ALSA、系统服务或 LiteUI 接口。
- 初始化、completion 或 returned length 损坏必须沿 adapter 唯一 reset/failure path 完成全部 waiter。
- capture rxq 不属于首期数据路径；规范要求的 device discovery 与 event queue 仍不得伪造或跳过。
