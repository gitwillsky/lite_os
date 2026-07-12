# LiteOS

LiteOS 是一个使用 Rust `no_std` 实现的 RISC-V 64 操作系统基线，当前目标平台是 QEMU `virt`。项目优先保证特权级、SMP、内存、调度、设备和 Linux/riscv64 ABI 的已声明子集正确，不以功能数量作为兼容性证明。

## 当前边界

- M-mode bootloader + S-mode kernel + U-mode 静态 BusyBox `init + ash` 三层结构。
- QEMU `virt`，RV64GC；实际 hart 集合来自 DTB，容量受可用内存和 SBI/PLIC/CLINT 表达能力约束。
- bootloader 只把 cold-boot hart 送入 kernel；kernel 构造动态 hart topology 后通过 SBI HSM 启动 secondary。
- Sv39、统一 VMA owner、独立用户地址空间、页级 W^X、栈 guard page、`brk` 与 eager anonymous private `mmap/munmap/mprotect`。
- 显式 Process/Thread/SchedulingEntity 边界；支持 fork-shaped process clone、共享资源 thread clone、TLS、clear-child-tid、futex 与 robust-list cleanup。
- 唯一 CFS-like vruntime runqueue、timer preemption、SMP mailbox 与统一 indexed wait registry。
- 同步读写 ext2 revision 1 启动卷与 boot-time device filesystem mount；进程通过统一 fd/OFD 模型访问 regular file、directory、pipe 与 `/dev/null|zero|tty|console`。
- PLIC、Goldfish RTC、VirtIO MMIO legacy block；设备只服务启动路径。
- 静态 ELF64 `ET_EXEC`，Linux 形式 `argc/argv/envp/auxv` 初始栈；固定 musl v1.2.6 pthread create/join、mutex/condition/timedwait、signal interruption 与定向 `SA_RESTART` consumer 已纳入冷启动围栏。
- 57 个 Linux/riscv64 syscall number；UART/TTY、anonymous pipe、ppoll readiness、system reset 与文件入口统一经过各自的单一 owner，未实现编号返回 `-ENOSYS`。

LiteOS 当前不支持 futex PI/requeue/bitset、完整 clone/vfork flags、所有 syscall 的 restart、signal altstack/realtime queue/process-directed delivery、socket/通用 IPC、file/shared/lazy mapping、PIE、动态链接、完整 pthread runtime、GUI、network userspace 或任意 musl 程序。当前只对 blocking `wait4` 和无 timeout 的 futex WAIT 实现 `SA_RESTART`；固定 BusyBox/pthread consumer 通过不外推为通用 musl 兼容。项目不声称完整 Linux 兼容、POSIX.1-2024 符合或 musl 兼容。

## 组件

- `bootloader/`：M-mode RustSBI firmware，负责 DTB、UART/CLINT、HSM、TIME、IPI、RFENCE、SRST 和 DBCN。
- `kernel/`：S-mode kernel，负责 trap/SMP、内存、task/scheduler、VFS/ext2、PLIC/VirtIO/RTC 和 syscall dispatch。
- `syscall-abi/`：kernel dispatcher 使用的 Linux/riscv64 syscall number。
- `user/`：固定 BusyBox config/inittab 与 musl ABI consumer；不包含自有 runtime 或第二个 init。

完整调用链、所有权和不支持范围见 [当前架构](docs/architecture.md)；syscall 精确状态见 [Linux/riscv64 syscall 支持矩阵](docs/syscall-support.md)。

## ABI 与规范基线

- Linux/riscv64 UAPI：Linux `v7.1`，commit `8cd9520d35a6c38db6567e97dd93b1f11f185dc6`。
- POSIX 语义目标：POSIX.1-2024 / Issue 8。
- RISC-V ELF psABI：2026-07-01 的 `e03d44ae2f0e1144f9498c2896b5ae25b0449398`。
- RISC-V Privileged Architecture：`v20260120`。
- SBI：`v3.0` 用于审计当前实现的有效子集；firmware 对外由 RustSBI 报告 SBI `2.0`。
- VirtIO：`1.4` CS01。
- musl：`v1.2.6` 只作为 ABI consumer 审计对象。
- BusyBox：官方 release `1.37.0`，是默认且唯一的 `init + ash` userspace/rootfs。

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
# 构建 bootloader、kernel、固定 musl/BusyBox，并生成 ext2 镜像
make build

# 以默认的 8-hart QEMU 配置启动；8 是运行示例，不是 kernel 容量常量
make run
```

成功启动后会看到 RustSBI/kernel 日志、BusyBox init 启动信息和 UART console 激活提示；按 Enter 后进入 ash：

```text
Please press Enter to activate this console.
~ #
```

镜像固定为 ext2 revision 1、4 KiB block、256-byte inode，并只启用驱动完整处理的 `filetype,sparse_super,large_file` 特性；不会依赖宿主机随版本变化的 mke2fs 默认 feature 集合。

默认 rootfs 只包含固定 config 选中的 BusyBox applet；全部入口是同一 ELF inode 的 hardlink。

分组构建：

```bash
make build-bootloader
make build-kernel
make build-musl
make build-rootfs
```

`create_fs.py` 是 rootfs builder 的底层工具，必须显式传入 `--init`；默认入口是 `make build-rootfs`，它还会写入 inittab 和 BusyBox hardlink applet。

固定 musl v1.2.6 pthread 验收可单独执行：

```bash
make verify-musl
```

该目标校验官方 release tarball SHA-256，在 `target/musl-static/` 构建一次性工具链产物，将 `user/musl-smoke.c` 链接为静态 `ET_EXEC`，再以独立 ext2 镜像冷启动并要求输出 `LiteOS musl pthread signal ok`。consumer 实际执行 `pthread_create/join`、mutex/condition/timedwait，以及 `tgkill` 分别中断 futex、`nanosleep` 和 `waitpid` 的 handler/`EINTR`/`rem`/reap 路径；随后启用 `SA_RESTART`，验证无 timeout futex 和 `waitpid` 透明重放而 `nanosleep` 仍返回 `EINTR`。musl 源码和二进制不进入仓库。

musl source、sysroot 和 smoke ELF 分别使用包含官方 SHA-256、compiler identity、configure/link recipe 和 consumer hash 的 content fingerprint。缓存以不可变 generation 生成，并用进程锁与原子 symlink 切换发布；普通命中不重新 configure/build/install。冷构建并行度默认来自宿主 CPU/上层 GNU Make jobserver，可用 `LITEOS_BUILD_JOBS=<n>` 显式覆盖。强制重建使用 `python3 scripts/verify_musl.py --build-only --rebuild`；清理全部 generation 使用 `make clean-musl`。

固定 BusyBox 1.37.0 `init + ash` 验收可单独执行：

```bash
make verify-busybox
```

该目标校验 BusyBox 官方 tarball SHA-256，应用唯一 `user/busybox.config` 与 `user/inittab`，构建 RISC-V 静态 `ET_EXEC` 和单 inode hardlink applet rootfs。gate 验证 ash 算术、pipeline、重定向、后台 wait、VINTR 和同镜像 1-hart 写入/8-hart 冷启动读回；同时检查无 dynamic interpreter、无 W+X LOAD、用户栈不可执行。BusyBox 源码和二进制不进入仓库。

BusyBox source 与静态 ELF 分别按官方 archive SHA-256、唯一 config fragment、musl sysroot fingerprint、compiler identity 和构建 recipe 做内容寻址缓存。命中后仍执行 ELF 围栏，并从缓存 ELF 重新构造指定 ext2 image；强制重建使用 `python3 scripts/verify_busybox.py --build-only --image fs.img --rebuild`，清理全部 BusyBox generation 使用 `make clean-busybox`。并行度与 musl gate 共用 `LITEOS_BUILD_JOBS` 规则。

## 调试

```bash
# 窗口 1
make run-gdb

# 窗口 2
make gdb
```

## 验证约束

本仓库不维护、不修正、不执行测试用例。统一运行 `make verify`；它执行 AST 架构围栏、workspace/组件构建、Clippy、ELF 静态检查、默认 BusyBox rootfs 的 `-smp 1/3/8` 冷启动、固定 musl pthread consumer，以及 BusyBox `init + ash` UART/持久化 gate。各阶段的证据保存在 `docs/phase-*.md`。
