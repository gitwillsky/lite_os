# LiteOS

LiteOS 是一个使用 Rust `no_std` 实现的 RISC-V 64 操作系统基线，当前目标平台是 QEMU `virt`。项目优先保证特权级、SMP、内存、调度、设备和 Linux/riscv64 ABI 的已声明子集正确，不以功能数量作为兼容性证明。

## 当前边界

- M-mode bootloader + S-mode kernel + U-mode 动态 PIE BusyBox `init + ash` 三层结构。
- QEMU `virt`，RV64GC；实际 hart 集合来自 DTB，容量受可用内存和 SBI/PLIC/CLINT 表达能力约束。
- bootloader 只把 cold-boot hart 送入 kernel；kernel 构造动态 hart topology 后通过 SBI HSM 启动 secondary。
- Sv39、统一 VMA owner、独立用户地址空间、页级 W^X、栈 guard page、`brk`，以及 eager anonymous/file private `mmap/munmap/mprotect` 与 `MAP_FIXED`。
- 显式 Process/Thread/SchedulingEntity 边界；支持 fork-shaped process clone、共享资源 thread clone、TLS、clear-child-tid、futex 与 robust-list cleanup。
- 唯一 CFS-like vruntime runqueue、timer preemption、SMP mailbox 与统一 indexed wait registry。
- 同步读写 ext2 revision 1 启动卷与 boot-time device filesystem mount；进程通过统一 fd/OFD 模型访问 regular file、directory、pipe 与 `/dev/null|zero|tty|console`。
- PLIC、Goldfish RTC、VirtIO MMIO legacy block 与 virtio-rng；设备只服务已声明路径。
- ELF64 ET_EXEC 与动态 PIE/PT_INTERP，共享 musl loader、RELRO、TLS、`AT_BASE/AT_RANDOM/AT_EXECFN`；固定 pthread consumer、动态 BusyBox 与 `dlopen` 共享对象均纳入冷启动围栏。
- 63 个 Linux/riscv64 syscall number；UART/TTY、anonymous pipe、ppoll、getrandom、sysinfo、system reset 与文件入口统一经过各自的单一 owner，未实现编号返回 `-ENOSYS`。

LiteOS 当前不支持 futex PI/requeue/bitset、完整 clone/vfork flags、所有 syscall 的 restart、signal altstack/realtime queue、socket/通用 IPC、shared/lazy mapping、完整 pthread runtime、GUI、network userspace 或任意 musl 程序。动态链路证明固定 BusyBox（含基础 job control）与 `dlopen` consumer，不外推为完整 Linux、POSIX.1-2024 或 musl conformance。

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

当前工具集除 `init + ash` 与基础文件命令外，还包含 `awk/sed`、`head/tail/cut/sort/uniq/tr/tee`、`find`、`basename/dirname/expr/seq/sleep`、`gzip/gunzip/zcat` 与 `sha256sum`。每个入口都由 BusyBox UART gate 执行真实文件、pipe、压缩或校验路径，不以“编译进 ELF”代替运行支持。

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

该目标在 `target/musl-runtime/` 构建唯一 musl sysroot（`libc.a/libc.so/ld-musl` 与 Linux 7.1 UAPI），并将 `user/musl-smoke.c` 作为独立静态 ABI consumer 冷启动。consumer 验证 pthread、futex、signal interruption 与定向 `SA_RESTART`；musl 源码和二进制不进入仓库。

musl source、sysroot 和 smoke ELF 分别使用包含官方 SHA-256、compiler identity、configure/link recipe 和 consumer hash 的 content fingerprint。缓存以不可变 generation 生成，并用进程锁与原子 symlink 切换发布；普通命中不重新 configure/build/install。冷构建并行度默认来自宿主 CPU/上层 GNU Make jobserver，可用 `LITEOS_BUILD_JOBS=<n>` 显式覆盖。强制重建使用 `python3 scripts/verify_musl.py --build-only --rebuild`；清理全部 generation 使用 `make clean-musl`。

固定 BusyBox 1.37.0 `init + ash` 验收可单独执行：

```bash
make verify-busybox
```

该目标构建 RISC-V 动态 PIE BusyBox 和单 inode hardlink applet rootfs，检查标准 musl interpreter、NEEDED libc、NOW/RELRO、W^X 与 NX stack。gate 还执行 `dlopen/dlsym/dlclose`、getrandom、ash、pipeline、后台 wait、VINTR 和 1-hart 写入/8-hart 冷启动读回。

BusyBox source、动态 ELF、unstripped diagnostics 与共享对象 probe 按官方 archive SHA-256、唯一 config、musl sysroot fingerprint、toolchain identity 和 recipe 做内容寻址缓存。命中后仍执行 ELF 围栏并重新构造镜像；强制重建使用 `python3 scripts/verify_busybox.py --build-only --image fs.img --rebuild`。

## 调试

```bash
# 窗口 1
make run-gdb

# 窗口 2
make gdb
```

## 验证约束

本仓库不维护、不修正、不执行测试用例。统一运行 `make verify`；它执行 AST 架构围栏、workspace/组件构建、Clippy、ELF 静态检查、默认 BusyBox rootfs 的 `-smp 1/3/8` 冷启动、固定 musl pthread consumer，以及 BusyBox `init + ash` UART/持久化 gate。各阶段的证据保存在 `docs/phase-*.md`。
