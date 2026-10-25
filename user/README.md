# LiteOS userspace

`user/` 是用户态的单一 Cargo workspace。目录按**是什么**分开，打开任何一个包都有 README 说明它是程序还是库、做什么、对应 Windows 2000 的哪个组件：

| 目录 | 是什么 | 说明 |
|---|---|---|
| [`programs/`](programs/README.md) | **程序**（可执行文件） | compositor、logon、desktop、file-explorer、control-panel、task-manager、volume-control |
| [`libraries/`](libraries/README.md) | **库**（目标系统上是共享库，类似 DLL） | 运行时、图形、控件、Wayland、Linux 接口封装、桌面辅助库 |
| [`base/`](base/README.md) | **系统文件与配置** | passwd、inittab、桌面条目、BusyBox 配置等，不是代码 |
| [`diagnostics/`](diagnostics/README.md) | **诊断程序**（C） | cputest/memtest/cachetest，仅用于验证 |

产品由 BusyBox init、网络服务、Wayland compositor、可信 logon、用户 desktop 与一组独立程序组成；UART 保留恢复 shell。
首次账户设置与验证见[桌面验证](../docs/development/desktop-verification.md)。

动态链接：程序与共享库共用 `liteos-runtime`（承载 `std`，对应 msvcrt.dll），安装在 `/usr/lib/liteos/`。
设计与限制见[用户态架构](../docs/architecture/userspace-abi.md)，构建流程见[构建与验证](../docs/development/build-and-verify.md)。

音频混音归属 kernel ALSA；用户态没有音频服务或私有音频协议。接口与失败路径见
[用户态契约](../docs/architecture-contract/userspace-abi.md)和[音频契约](../docs/architecture-contract/audio.md)。
