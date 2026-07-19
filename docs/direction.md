# LiteOS 产品方向

本文件是产品方向决策的唯一 owner，描述目标状态与生长纪律。当前实现状态以 [architecture.md](architecture.md) 及其领域文档为准；两者冲突时，本文档描述的是目标，不是现状。

## 产品身份

面向极客的键盘驱动工作站。判据是"每天能用"：不为 benchmark 数字牺牲可用性，不为理念纯粹牺牲软件生态。

## 交互范式

- 终端即桌面：整个 UI 是 GPU/CPU 光栅化的终端环境，不存在独立"窗口系统"。
- 分屏与 surface 管理长在显示层；不引入用户态终端复用器层（tmux 式方案是兼容应用，不是系统机制）。
- surface 抽象预留非文本 buffer（内联图像等）的形状，但 GUI 应用不做一等公民。

## Linux ABI 纪律

Linux ABI 冻结为终端应用兼容层：现有审计子集服务 CLI/TUI 生态（编辑器、git、工具链经 APK 进入），
停止向桌面 Linux 方向生长（不做 X11/Wayland 协议、不追 GUI toolkit 兼容）。
每次 ABI 扩张的判据是"终端应用需要吗"。ABI 现状与矩阵仍由 [syscall-support.md](syscall-support.md) 维护。

## 原生 shell

shell 是原生自建的系统界面，范围刻意收窄：

- 极致的交互细节：行编辑、补全、零配置可用，渲染与显示层同管线。
- 结构化系统自省：进程、surface、调度、内存与性能计数器可查询、可脚本化，查询接口从 kernel 干净长到 shell 补全。
- 传统文本应用原样运行；shell 不追求现存 shell 的功能全集。

## 硬件目标

- 开发期目标是 QEMU `virt`；真实硬件是显示层 v1 之后的里程碑：一块确定的 RISC-V 板（内存 ≥4GB，型号待定），framebuffer 直驱 + USB HID 输入，不含 GPU 驱动。
- 不为硬件多样性投入：新增平台必须按架构契约新增隔离的 `arch`/`platform` backend。

## 性能目标

- 真实板上 boot-to-shell <1s；input-to-pixel 延迟 p50 <8ms、p99 <16.6ms（60Hz 一帧之内）。
- QEMU 中 whole-machine 数字作趋势诊断；上板后转为板载 blocking gate。门禁机制（blocking vs diagnostic、阈值规则）归属 [development/build-and-verify.md](development/build-and-verify.md)；交互热路径的逐跳 benchmark 决策随实现落地时更新该文件。

## 演进顺序

1. input→pixel 全链路垂直切片，逐跳延迟插桩（中断 → evdev → shell → PTY → 渲染 → 合成 → 翻页），使性能目标从第一天可测量。
2. 显示层 multiplexer：分屏与 surface 抽象。
3. 原生 shell v1，迁移到新显示管线。

冻结（非砍掉，排队到上述里程碑之后）：网络新功能、APK 生态扩张、SMP 扩展、新文件系统。现有能力维持可用。
