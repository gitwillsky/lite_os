# LiteOS

LiteOS 是一个使用 Rust `no_std` 实现的 RISC-V 64 操作系统基线，当前目标平台是 QEMU `virt`。项目优先保证特权级、SMP、内存、调度、设备和 Linux/riscv64 ABI 的已声明子集正确，不以功能数量作为兼容性证明。

## 当前边界

- M-mode bootloader + S-mode kernel + U-mode 静态 init 三层结构。
- QEMU `virt`，RV64GC；实际 hart 集合来自 DTB，容量受可用内存和 SBI/PLIC/CLINT 表达能力约束。
- bootloader 只把 cold-boot hart 送入 kernel；kernel 构造动态 hart topology 后通过 SBI HSM 启动 secondary。
- Sv39、统一 VMA owner、独立用户地址空间、页级 W^X、栈 guard page、`brk` 与 eager anonymous private `mmap/munmap/mprotect`。
- 显式 Process/Thread/SchedulingEntity 边界；支持 fork-shaped process clone、共享资源 thread clone、TLS、clear-child-tid、futex 与 robust-list cleanup。
- 唯一 CFS-like vruntime runqueue、timer preemption、SMP mailbox 与统一 indexed wait registry。
- 同步读写 ext2 revision 1 启动卷；进程通过统一 fd/OFD 模型访问 console、普通文件和目录。
- PLIC、Goldfish RTC、VirtIO MMIO legacy block；设备只服务启动路径。
- 静态 ELF64 `ET_EXEC`，Linux 形式 `argc/argv/envp/auxv` 初始栈；固定 musl v1.2.6 pthread create/join、mutex/condition/timedwait 与 signal-interrupted nanosleep consumer 已纳入冷启动围栏。
- 39 个 Linux/riscv64 syscall number；文件入口统一经过 fd/OFD 与 VFS，未实现编号统一返回 `-ENOSYS`。

LiteOS 当前不支持 futex PI/requeue/bitset、完整 clone/vfork flags、`SA_RESTART`、signal altstack/queue/process-directed delivery、pipe/socket/IPC、file/shared/lazy mapping、PIE、动态链接、完整 pthread runtime、shell、GUI、network userspace 或常规 musl 程序。固定 pthread consumer 通过不外推为通用 musl 兼容。项目不声称完整 Linux 兼容、POSIX.1-2024 符合或 musl 兼容。

## 组件

- `bootloader/`：M-mode RustSBI firmware，负责 DTB、UART/CLINT、HSM、TIME、IPI、RFENCE、SRST 和 DBCN。
- `kernel/`：S-mode kernel，负责 trap/SMP、内存、task/scheduler、VFS/ext2、PLIC/VirtIO/RTC 和 syscall dispatch。
- `syscall-abi/`：kernel 与自带 user runtime 共享的 Linux/riscv64 syscall number。
- `user/`：`_start`、panic/output、Linux/riscv64 raw syscall wrapper 和最小 `/bin/init`。

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
sudo apt-get install qemu-system-misc e2fsprogs binutils-riscv64-unknown-elf gcc-riscv64-linux-gnu
```

## 构建与启动

```bash
# 构建 bootloader、kernel、user，并生成 ext2 镜像
make build

# 以默认的 8-hart QEMU 配置启动；8 是运行示例，不是 kernel 容量常量
make run
```

成功启动后会看到 RustSBI/kernel 日志与：

```text
LiteOS init
vma ok
process ok
thread futex ok
signal ok
ext2 rw ok
```

镜像固定为 ext2 revision 1、4 KiB block、256-byte inode，并只启用驱动完整处理的 `filetype,sparse_super,large_file` 特性；不会依赖宿主机随版本变化的 mke2fs 默认 feature 集合。

当前没有 shell 或交互提示符；init 在输出后持续 `sched_yield`。

分组构建：

```bash
make build-bootloader
make build-kernel
make build-user
python3 create_fs.py create
```

`create_fs.py` 只创建 128 MiB、4 KiB block 的 ext2 启动卷，只允许 `/bin/init` 进入镜像，并将其 mode 设为 `0755`。

固定 musl v1.2.6 pthread 验收可单独执行：

```bash
make verify-musl
```

该目标校验官方 release tarball SHA-256，在 `target/musl-static/` 构建一次性工具链产物，将 `user/musl-smoke.c` 链接为静态 `ET_EXEC`，再以独立 ext2 镜像冷启动并要求输出 `LiteOS musl pthread signal ok`。consumer 实际执行 `pthread_create/join`、mutex/condition/timedwait，以及 `tgkill` 分别中断 futex、`nanosleep` 和 `waitpid` 的 handler/`EINTR`/`rem`/reap 路径；musl 源码和二进制不进入仓库。

## 调试

```bash
# 窗口 1
make run-gdb

# 窗口 2
make gdb
```

## 验证约束

本仓库不维护、不修正、不执行测试用例。统一运行 `make verify`；它执行 AST 架构围栏、workspace/组件构建、Clippy、ELF 静态检查、`-smp 1/3/8` Rust init 冷启动，以及固定 musl pthread consumer 冷启动 gate。各阶段的证据保存在 `docs/phase-*.md`。
