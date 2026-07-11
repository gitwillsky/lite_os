# Phase 13：最终收敛与验收

日期：2026-07-11（Asia/Shanghai）

## 1. 收敛结果

Phase 0–13 已把 LiteOS 收敛为 QEMU `virt` 上可验证的 RISC-V 64 / Rust `no_std` 基线：

- 三组件边界固定为 M-mode bootloader、S-mode kernel 和唯一静态用户程序 `/bin/init`；
- kernel 使用 Sv39、8-hart 启动、per-hart scheduler、只读 ext2 和 VirtIO block；
- userspace 只暴露 12 个 Linux/riscv64 syscall number，8 个 Complete、4 个 Partial；
- 不完整的私有 syscall、GUI、FAT32、signal/futex/IPC、伪 fd、动态 ELF、写文件系统和未接线驱动已整链删除；
- 当前事实由 [architecture.md](architecture.md) 与 [syscall-support.md](syscall-support.md) 统一描述，Phase 文档只保留历史决策记录。

## 2. Phase 13 代码收敛

最终阶段只清理仍会误导维护者或构建入口的残留：

1. kernel 删除未使用的 `IrqRwLock`、logger 配置入口、page-table accessor 与 import，workspace warning 从 9 个降为 0；
2. bootloader 删除未使用的 full-trap continuation/context-switch façade 与 ACLINT read/SSWI façade，warning 从 19 个降为 0；
3. Makefile 删除 ELF-to-bin、`killall` timeout、未支持的 VirtIO RNG/network 设备，`run`/`run-gdb` 统一依赖 bootloader、kernel、user 和 ext2 镜像；
4. `create_fs.py` 只装入固定的 `target/riscv64gc-unknown-none-elf/release/init`，不再扫描或兼容旧 `.bin` 产物；
5. README、AGENTS 与 CLAUDE 入口改为当前能力、限制和验证方法，不再把历史功能当成现状。

## 3. 最终验证

仓库规则禁止维护、修正或执行测试用例；本阶段没有运行测试。

### 3.1 静态与构建

- `git diff --check`：通过；
- `cargo fmt --all -- --check`：通过；
- `cargo check --workspace`：通过，0 warning；
- `make build-user`：通过，0 warning；
- `make build-kernel`：通过，0 warning；
- `make build-bootloader`：通过，0 warning；
- `python3 create_fs.py create`：通过，`/bin/init` 是 regular inode，mode `0755`；
- `make -n run`：完整展开 bootloader/kernel/user/image 依赖与唯一 VirtIO block 配置。

### 3.2 ELF

`init` 静态检查结果：

- ELF64、little-endian、RISC-V、`ET_EXEC`；
- flags 为 RVC + double-float ABI；
- program header table 位于首个 `R E` LOAD；
- GNU stack 为 `RW`，不可执行；
- `_start` 先初始化 `gp`，再把初始 `sp` 交给 `__user_start`。

### 3.3 QEMU

使用最终 Makefile 等价参数进行两次独立 8-hart 冷启动：

| 轮次 | Boot HART | 结果 |
|---:|---:|---|
| 1 | 5 | 8 cores started；RTC、VirtIO block、ext2、PID 1 初始化；输出 `LiteOS init`。 |
| 2 | 5 | 8 cores started；RTC、VirtIO block、ext2、PID 1 初始化；输出 `LiteOS init`。 |

两轮恰好选择同一个 Boot HART，因此该观察只证明两次完整启动路径，不声称覆盖不同 boot owner 或并发交错。

## 4. 最终边界

本轮完成的是可读、可构建、可启动且契约诚实的最小基线，不是完整 Linux/POSIX/musl 实现。后续功能只能按 [architecture.md 的路线](architecture.md#16-后续路线) 从标准 ABI 与唯一内部模型重新增加；不得恢复已删除的私有编号、错签名 handler 或半实现兼容层。
