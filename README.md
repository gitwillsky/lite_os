# LiteOS

LiteOS 是一个使用 Rust `no_std` 实现的 RISC-V 64 操作系统基线，当前目标平台是 QEMU `virt`。项目优先保证特权级、SMP、内存、调度、设备和 Linux/riscv64 ABI 的已声明子集正确，不以功能数量作为兼容性证明。

## 当前边界

- M-mode bootloader + S-mode kernel + U-mode 静态 init 三层结构。
- QEMU `virt`，RV64GC，最多 8 个 hart。
- Sv39、独立用户地址空间、页级 W^X、栈 guard page、`brk`。
- 显式 Process/Thread/SchedulingEntity 边界，但当前只有 PID 1 的单进程单线程模型。
- 唯一 CFS-like vruntime runqueue、timer preemption、SMP mailbox 与 deadline sleep queue。
- 只读 ext2 启动卷；没有用户可见的通用 fd/OFD 模型。
- PLIC、Goldfish RTC、VirtIO MMIO legacy block；设备只服务启动路径。
- 静态 ELF64 `ET_EXEC`，Linux 形式 `argc/argv/envp/auxv` 初始栈。
- 12 个 Linux/riscv64 syscall number；8 个 Complete，4 个明确 Partial。未实现编号统一返回 `-ENOSYS`。

LiteOS 当前不支持 fork/clone/wait、signal、futex、pipe/socket/IPC、通用文件 I/O、`mmap`、PIE、动态链接、TLS、shell、GUI、network userspace 或常规 musl 程序。项目不声称完整 Linux 兼容、POSIX.1-2024 符合或 musl 兼容。

## 组件

- `bootloader/`：M-mode RustSBI firmware，负责 DTB、UART/CLINT、HSM、TIME、IPI、RFENCE、SRST 和 DBCN。
- `kernel/`：S-mode kernel，负责 trap/SMP、内存、task/scheduler、VFS/ext2、PLIC/VirtIO/RTC 和 syscall dispatch。
- `syscall-abi/`：kernel 与自带 user runtime 共享的 Linux/riscv64 syscall number。
- `user/`：`_start`、panic/output、三个 raw syscall wrapper 和最小 `/bin/init`。

完整调用链、所有权和不支持范围见 [当前架构](docs/architecture.md)；syscall 精确状态见 [Linux/riscv64 syscall 支持矩阵](docs/syscall-support.md)。

## ABI 与规范基线

- Linux/riscv64 UAPI：Linux `v7.1`，commit `8cd9520d35a6c38db6567e97dd93b1f11f185dc6`。
- POSIX 语义目标：POSIX.1-2024 / Issue 8。
- RISC-V ELF psABI：2026-07-01 的 `e03d44ae2f0e1144f9498c2896b5ae25b0449398`。
- RISC-V Privileged Architecture：`v20260120`。
- SBI：`v3.0` 用于审计当前实现的有效子集；firmware 对外由 RustSBI 报告 SBI `2.0`。
- VirtIO：`1.4` CS01。
- musl：`v1.2.6` 只作为 ABI consumer 审计对象。

不可变 revision 和一手来源见 [规范基线](docs/standards-baseline.md)。

## 环境

- Rust nightly `nightly-2025-06-15`，由 `rust-toolchain.toml` 固定。
- target `riscv64gc-unknown-none-elf`。
- QEMU `qemu-system-riscv64`。
- e2fsprogs：`mke2fs` 和 `debugfs`。
- Rust `llvm-tools`；GDB/addr2line 只在调试时需要。

macOS 可使用：

```bash
brew install qemu e2fsprogs
brew tap riscv/riscv
brew install riscv-gnu-toolchain
```

Debian/Ubuntu 可使用：

```bash
sudo apt-get install qemu-system-misc e2fsprogs binutils-riscv64-unknown-elf
```

## 构建与启动

```bash
# 构建 bootloader、kernel、user，并生成 ext2 镜像
make build

# 启动 QEMU virt / 8 hart
make run
```

成功启动后会看到 RustSBI/kernel 日志与：

```text
LiteOS init
```

当前没有 shell 或交互提示符；init 在输出后持续 `sched_yield`。

分组构建：

```bash
make build-bootloader
make build-kernel
make build-user
python3 create_fs.py create
```

`create_fs.py` 只创建 128 MiB、4 KiB block 的 ext2 启动卷，只允许 `/bin/init` 进入镜像，并将其 mode 设为 `0755`。

## 调试

```bash
# 窗口 1
make run-gdb

# 窗口 2
make gdb
```

## 验证约束

本仓库不维护、不修正、不执行测试用例。当前验证使用 workspace/组件构建、ELF/反汇编检查与非测试 QEMU 冷启动观察。各阶段的证据保存在 `docs/phase-*.md`。
