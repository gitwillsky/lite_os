# LiteOS 当前架构

LiteOS 是架构中立的 Rust `no_std` kernel。通用代码只消费编译期静态 `arch` 与 `platform` façade；AArch64 + QEMU `virt` + HVF 是 Apple Silicon 上的 first-class backend，RISC-V64 + QEMU `virt` 作为保留 backend 持续通过编译、静态和启动门禁。

## 全局设计

1. `main.rs` 是唯一 composition root，只决定初始化顺序、adapter 装配和 fail-stop policy。
2. `arch` 隐藏指令集 execution mechanism；`platform` 隐藏 machine、firmware 与设备装配。
3. `entry` 将 raw boot/trap ABI 转成 typed value；generic `trap` 只处理语义事件。
4. 每个复合状态有且仅有一个 owner；其他 module 通过窄 façade 请求操作或读取不可变快照。
5. syscall 层只处理 Linux 64-bit 编号、UAPI codec、user-copy 和 errno；寄存器、ELF 与 signal frame 由编译期 architecture ABI backend 拥有。
6. 同一能力只保留一条生产路径；不提供私有 ABI、兼容入口或运行时 backend 分派。

## 领域导航

| 领域 | 当前事实 |
|---|---|
| 启动与平台 | [boot 与 machine assembly](architecture/boot-platform.md) |
| 执行 | [arch、entry、CPU、trap、timer、sync](architecture/execution.md) |
| 内存 | [frame、page table、VMA、user-copy](architecture/memory.md) |
| 进程 | [Process、Thread、scheduler、signal、wait](architecture/process-scheduling.md) |
| 存储 | [VFS、OFD、ext2、page cache](architecture/filesystem-storage.md) |
| 通信 | [Pipe、epoll、socket 与 network](architecture/ipc-network.md) |
| 设备 | [VirtIO、DRM、evdev、PTY 与 terminal](architecture/devices-terminal.md) |
| 图形会话 | [compositor、LiteUI、React desktop 与窗口应用](architecture/lite-ui.md) |
| 用户态 | [ELF、musl、BusyBox、APK 与 ABI](architecture/userspace-abi.md) |

依赖矩阵、state owner 和 interface 约束在 [架构契约](architecture-contract.md)；用户可观察 ABI 状态在 [syscall 支持矩阵](syscall-support.md)。

## 当前边界

- first-class 产品范围只覆盖 Apple Silicon 上的 AArch64 QEMU `virt`、HVF 和 `-cpu host`；TCG 是显式诊断 fallback，不允许静默替代 HVF。
- RISC-V64 代码与 RustSBI 启动链保留，但不承担默认的完整应用性能门禁。两个 backend 之外的新目标必须新增相互隔离的 `arch`/`platform` 实现。
- Linux ABI 是经过明确矩阵审计的子集，不等于完整 Linux、POSIX 或任意 musl 程序兼容。
- 缺失的语义必须返回标准错误或保持接口不存在，不能用 stub、忽略 flags 或静默降级冒充完成。
