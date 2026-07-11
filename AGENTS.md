# LiteOS 工程指南

LiteOS 是 Rust `no_std` 的 RISC-V 64 操作系统基线，当前目标平台是 QEMU `virt`。实际 hart 集合来自 DTB，容量受可用内存和 SBI/PLIC/CLINT 表达能力约束。它只实现明确列出的 Linux/riscv64 ABI 子集，不声称完整 Linux、POSIX 或 musl 兼容。

开始修改前读取：

- `README.md`：当前功能与构建入口。
- `docs/architecture.md`：当前调用链、所有权和不支持范围。
- `docs/syscall-support.md`：当前 syscall 状态。
- `docs/standards-baseline.md`：固定规范/revision。

## 三个组件

- `bootloader/`：M-mode RustSBI firmware；DTB、UART/CLINT、HSM、TIME/IPI/RFENCE/SRST/DBCN。
- `kernel/`：S-mode kernel；trap/SMP、Sv39、task/scheduler、只读 ext2、PLIC/VirtIO block/RTC、syscall。
- `user/`：静态 `_start`、panic/output、最小 wrapper 和唯一 `/bin/init`。

当前没有 fork/clone/wait、signal、futex、IPC、通用 fd/OFD、可写文件系统、`mmap`、PIE/dynamic ELF、TLS、shell、GUI 或 network userspace。不得为旧功能恢复私有 syscall 或同名近似实现；未完成标准语义时保持 `-ENOSYS`。

## 命令

```bash
make build
make build-bootloader
make build-kernel
make build-user
python3 create_fs.py create
make run
```

toolchain 由 `rust-toolchain.toml` 固定为 `nightly-2025-06-15`，target 为 `riscv64gc-unknown-none-elf`。镜像是 ext2，只包含 mode `0755` 的 `/bin/init`。

## 验证

仓库规则禁止维护、修正或执行测试用例。使用：

- `git diff --check`；
- `cargo check --workspace`；
- 三组件构建；
- `llvm-readelf` / `llvm-objdump` 静态检查；
- QEMU `virt` 使用不同 `-smp` 值做非测试冷启动，确认 online mask 等于 DTB mask、ext2、init 与 `LiteOS init`。

复杂逻辑的注释使用中文并按 `1. / 2. / 3.` 拆解；新 flag、缓存或特殊分支必须说明用意和缺失时的具体后果。修改保持最小、单路径，不保留 deprecated/feature-flag 双轨。
