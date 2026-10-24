# LiteOS

LiteOS 是一个以 Rust `no_std` 实现的多架构操作系统 kernel，配套 musl/BusyBox 用户态、APK 包管理和
UART shell；经典桌面按新计划实现中。通用 kernel 只通过编译期静态 `arch` 与 `platform` façade 消费硬件能力：

- **AArch64 + QEMU `virt` + HVF**：Apple Silicon 上的 first-class 产品路径，图形设备与内核启动画面经 UTM 运行。
- **RISC-V64 + QEMU `virt`**：保留 backend，持续通过编译、静态与启动门禁，不承担完整应用门禁。

项目追求清晰的状态所有权、窄接口、可证明的错误与清理路径，以及可持续执行的单元、性能和运行时验证。
没有实现的能力不会通过私有 ABI、兼容入口或双轨实现伪装为已支持。

## 第一次上手

只支持 Apple Silicon macOS。先手动安装两项需要交互授权的工具：

1. Xcode Command Line Tools：`xcode-select --install`
2. Homebrew：见 <https://brew.sh>

然后在仓库根目录执行：

```bash
make setup     # 安装 LLVM、QEMU、e2fsprogs、rustup 固定 toolchain、UTM
make build     # 构建 kernel、musl、BusyBox、OpenSSL与 rootfs 基线
make run-gui   # 在 UTM 窗口中显示内核启动画面，shell 位于串口
```

`make setup` 幂等，可重复执行；首次安装 rustup 后需新开 shell 让 `~/.cargo/bin` 进入 PATH。
首次 `make run-gui` 时 macOS 会询问是否允许自动化控制 UTM，需要允许。

## 日常开发循环

1. 修改代码前先读 [AGENTS.md](AGENTS.md)（开发规则）和对应领域的架构文档（见下方“文档导航”）。
2. 修改后运行 `make run-gui` 看效果：它增量构建 kernel，并同步系统文件到开发镜像，保留已装 APK 和用户数据。
3. 日常快速反馈用 `make verify-fast`（fmt、clippy、单元测试，不启动 QEMU）。
4. 提交前必须通过 `make verify`（快速门禁 + release/artifact/架构门禁 + 全部 runtime gate + RISC-V 次级门禁）。

### 常用命令

| 命令 | 用途 |
|---|---|
| `make setup` | 一次性准备 host 工具链与依赖 |
| `make build` | 构建全部产物与只读 rootfs 基线 `target/rootfs/<arch>.img` |
| `make run-gui` | UTM 图形设备与内核启动画面；Ctrl-C 只停止该 VM |
| `make run` | 无窗口 QEMU，串口在当前终端，用于非图形调试 |
| `make verify-fast` | host 静态检查与单元测试 |
| `make verify-runtime` | 构建一次后串行运行全部 QEMU/UTM runtime gate |
| `make verify` | 提交前完整门禁 |
| `make reset-rootfs` | 从基线重建开发镜像 `fs-<arch>.img`，**会清空其中用户数据** |
| `make prepare-agent-development` / `make run-agent-development` | 带 Node/npm 与 Codex/Claude CLI 的 Agent 开发镜像 |
| `make run-gdb` + `make gdb` | QEMU gdbstub 调试；gdb 需自行安装，`make setup` 不包含 |
| `make clean` | 清理构建产物 |

所有入口接受 `ARCH`（`aarch64`/`riscv64`）、`ACCEL`（`hvf`/`tcg`）、`PROFILE`（`release`/`debug`）、
`QEMU_MEMORY`（默认 `2G`）和 `QEMU_SMP`。RISC-V 必须显式指定 `ARCH=riscv64 ACCEL=tcg`。
完整参数、缓存规则与每个门禁的覆盖范围见 [构建、测试与验证](docs/development/build-and-verify.md)。

### 排查入口

- `make run-gui` 每次启动的完整串口输出覆盖写入 `target/logs/run-gui-serial.log`；桌面卡死、黑屏或退出先看这里。
- 构建失败时，脚本会打印失败命令及其输出尾部；下载类失败通常是网络问题，重跑即可命中已校验缓存。
- 所有外部下载（musl、OpenSSL、Alpine APK、npm 等）都由脚本中固定的版本与 SHA-256 校验，优先走国内镜像。
- Alpine stable 分支只保留每个包的最新 revision；若出现 Alpine 包 404，说明固定版本已被上游移除，
  需要在 `scripts/apk_*cache.py` 中升级版本与摘要，并同步 [规范基线](docs/standards-baseline.md)。

## 仓库结构

| 路径 | 内容 |
|---|---|
| `kernel/` | `no_std` kernel：`arch`/`platform` backend、内存、进程调度、VFS/ext4、IPC/socket、DRM/evdev/音频驱动、syscall |
| `syscall-abi/` | Linux 64-bit syscall 编号与 UAPI 定义 |
| `bootloader/` | RISC-V 启动链；AArch64 不需要 |
| `user/` | 基础 Rust 库、BusyBox init 配置与 diagnostics，见 [user/README.md](user/README.md) |
| `tools/` | `architecture-check`（架构/依赖/文档围栏）、`architecture-bench`、`kernel-unit`、`scheduler-unit` |
| `scripts/` | 构建、运行与门禁脚本；`workflow.py` 是唯一编排 owner，Makefile 只是稳定入口 |
| `assets/` | 经典桌面字体、光标、图标与启动画面 |
| `docs/` | 架构、契约、ABI 矩阵、规范基线与设计决策 |

## 开发规则速览

完整规则以 [AGENTS.md](AGENTS.md) 为准，最常碰到的几条：

- 通用 kernel 只经编译期 `arch`/`platform` façade 使用硬件；target `cfg`、CSR、汇编与页表编码只属于 `arch`。
- 每个复合状态只有一个 owner；不复制状态、不留兼容入口或双轨实现、不引入私有 ABI。
- 新能力必须对照固定的一手规范；范围缩减要写进 ABI 矩阵或当前架构限制。
- 接口、依赖或 owner 变化时同步更新对应契约文档；`architecture-check` 会自动检查依赖与文档归属。
- 单元、性能与运行时测试随行为一起维护；禁止通过修改阈值、基线或文档掩盖实现错误。

## 文档导航

- 所有文档的唯一索引与事实 owner：[docs/README.md](docs/README.md)
- 想通过阅读学习这个内核：从 [读码地图](docs/reading-guide.md) 开始
- 当前设计：[docs/architecture.md](docs/architecture.md)
- module、接口、依赖与状态 owner：[docs/architecture-contract.md](docs/architecture-contract.md)
- Linux 用户态 ABI 支持矩阵与已知缺口：[docs/syscall-support.md](docs/syscall-support.md)
- 固定的工具链、上游版本与来源：[docs/standards-baseline.md](docs/standards-baseline.md)

## 当前实现边界

旧图形用户态和用户态音频服务已拆除。当前 init 监督网络服务和 UART shell，显示器保留内核启动画面；
不提供登录、桌面应用、显示器文本 VT 或 seat 切换。kernel DRM/evdev、ALSA 混音和经典桌面素材保留。

新图形栈按[经典桌面与 Win32 运行时计划](docs/plans/classic-desktop.md)使用公开 Wayland/ALSA 协议实现。
音频尚缺多路真实输出和 amixer consumer 验证；新桌面尚未实现，不能把构建或内核门禁通过视作桌面验收。
