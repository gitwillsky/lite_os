# LiteOS 文档索引

本文件是仓库文档的唯一总索引。每个事实只由下列一个文档 owner 维护；其他文档只链接，不复制。

## 阅读入口

- [读码地图：从哪里开始理解这个内核](reading-guide.md)

## 架构

- [当前架构总则](architecture.md)
- [启动与平台](architecture/boot-platform.md)
- [执行、CPU、trap、timer 与同步](architecture/execution.md)
- [内存](architecture/memory.md)
- [进程与调度](architecture/process-scheduling.md)
- [文件系统与存储](architecture/filesystem-storage.md)
- [IPC 与网络](architecture/ipc-network.md)
- [设备与终端](architecture/devices-terminal.md)
- [音频](architecture/audio.md)
- [io_uring](architecture/io-uring.md)
- [内核崩溃与蓝屏](architecture/crash-dump.md)
- [用户态与 ABI](architecture/userspace-abi.md)
- [用户态 UI 运行库与 Rust SDK](architecture/ui-framework.md)

## 架构契约

- [全局 module、依赖与接口契约](architecture-contract.md)
- [启动与平台契约](architecture-contract/boot-platform.md)
- [崩溃诊断契约](architecture-contract/crash.md)
- [执行域契约](architecture-contract/execution.md)
- [内存契约](architecture-contract/memory.md)
- [进程与调度契约](architecture-contract/process-scheduling.md)
- [文件系统与存储契约](architecture-contract/filesystem-storage.md)
- [IPC 与网络契约](architecture-contract/ipc-network.md)
- [设备与终端契约](architecture-contract/devices-terminal.md)
- [音频契约](architecture-contract/audio.md)
- [io_uring 契约](architecture-contract/io-uring.md)
- [用户态与 ABI 契约](architecture-contract/userspace-abi.md)
- [UI 框架与 SDK 契约](architecture-contract/ui-framework.md)

## Linux 64-bit 用户态 ABI

- [syscall 支持总则](syscall-support.md)
- [进程与身份](syscall-support/process-identity.md)
- [内存](syscall-support/memory.md)
- [文件系统与 I/O](syscall-support/filesystem-io.md)
- [同步与调度](syscall-support/synchronization-scheduling.md)
- [信号与时间](syscall-support/signal-time.md)
- [IPC](syscall-support/ipc.md)
- [Socket](syscall-support/socket.md)
- [系统](syscall-support/system.md)

## 工程基线

- [固定规范与上游来源](standards-baseline.md)
- [构建、测试与验证](development/build-and-verify.md)
- [性能验证范围与成本契约](development/performance-coverage.md)
- [Rust UI SDK 与本机开发](development/ui-sdk.md)
- [Wine 运行时：构建、打包与安装](development/wine-runtime.md)
- [Wayland 与经典控件验证](development/wayland-ui-verification.md)
- [本机登录与桌面验证](development/desktop-verification.md)
- [生成的 scoped interface baseline](generated/architecture-interface.txt)

## 设计规格

- [经典桌面外观与交互规格（简体中文 Windows 2000 风格）](design/classic-desktop/README.md)
- [经典桌面外观与字体](design/classic-desktop/appearance.md)
- [经典桌面原版资源与导入](design/classic-desktop/assets.md)
- [经典桌面开机与登录](design/classic-desktop/boot-logon.md)
- [经典桌面资源管理器](design/classic-desktop/explorer.md)
- [经典桌面交互验收](design/classic-desktop/interaction.md)
- [经典桌面外壳](design/classic-desktop/shell.md)

## 进行中的计划

- [经典桌面与 Win32 运行时](plans/classic-desktop.md)
- [SDK、二进制兼容与系统内开发（实施中）](plans/classic-ui-sdk.md)
- [经典桌面应用框架（实施中）](plans/classic-ui-framework.md)
- [映射网络驱动器（SMB，实施中）](plans/network-drive.md)

## 设计决策与术语

- [首个声音里程碑只支持播放](decisions/audio-output-scope.md)
- [Native 音频边界使用 Linux ALSA PCM UAPI](decisions/alsa-pcm-native-abi.md)
- [首期音频硬件只使用 VirtIO Sound](decisions/virtio-sound-backend.md)
- [UI 交互类 runtime gate 在 UI 重构前移除](decisions/ui-interaction-gates-deferred.md)
- [术语表](glossary.md)

完成的计划、阶段快照和已被当前文档吸收的设计草案不保留在工作树中；需要追溯时使用 Git 历史。

## 领域所有权细节

- [领域依赖说明](architecture-contract/dependency-owners.md)
- [文件系统接口、事务与退休契约](architecture-contract/filesystem-transactions.md)
- [FUSE 契约](architecture-contract/fuse.md)
- [IPv4 与 TCP/UDP 传输契约](architecture-contract/ipv4-transports.md)
- [有序存储与源码审查](architecture-contract/ordered-storage.md)
- [Packet 与 Netlink 契约](architecture-contract/packet-netlink.md)
- [Pipe 与 readiness 契约](architecture-contract/pipe-readiness.md)
- [进程接口、等待与退休契约](architecture-contract/process-lifecycle.md)
- [System V IPC 对象与资源契约](architecture-contract/systemv-ipc.md)
- [UNIX socket 与辅助数据契约](architecture-contract/unix-sockets.md)
- [内核运行时覆盖](development/kernel-runtime-coverage.md)
