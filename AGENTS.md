# AGENTS.md

本文件为 Codex (Codex.ai/code) 在此代码仓库中工作时提供指导。

Linux musl 应用程序兼容的操作系统，使用 Rust 编写

## 项目架构

LiteOS 是一个用 Rust 编写的 RISC-V 64 位操作系统内核，设计上兼容 musl libc。代码库采用三组件架构：

### 核心组件

1. **引导加载器** (`bootloader/`): M-Mode SBI 实现，基于 RustSBI
   - 提供固件服务和 Hart 状态管理 (HSM)
   - 初始化硬件 (UART、CLINT、设备树解析)
   - 转换到 S-Mode 内核执行
   - 入口点：`bootloader/src/main.rs`

2. **内核** (`kernel/`): S-Mode 操作系统内核
   - 内存管理，支持多级页表 (`memory/`)
   - 任务管理，基于 fork/exec 进程模型 (`task/`)
   - VFS 虚拟文件系统，支持 FAT32 (`fs/`)
   - 全面的系统调用接口 (`syscall/`)，兼容 Linux 系统调用
   - VirtIO 驱动程序，支持块设备、控制台、GPU、网络设备 (`drivers/`)
   - 信号处理和 IPC 机制 (`signal/`, `ipc/`)
   - 入口点：`kernel/src/main.rs::kmain()`

3. **用户程序** (`user/`): 用户空间应用程序和 shell
   - `user/src/lib.rs` 中的最小运行时库
   - 用户程序以 ELF 二进制文件形式嵌入内核镜像

### 关键子系统

- **内存管理**: SLAB 分配器、帧分配器、页表虚拟内存
- **任务调度**: 多核 SMP 支持，工作窃取调度器
- **文件系统**: VFS 层，FAT32 实现，VirtIO 块设备驱动
- **系统调用**: Linux 兼容的系统调用接口，实现了 50+ 个调用
- **硬件抽象**: 设备树解析、VirtIO 设备支持、中断处理

## 开发命令

### 构建和运行

```bash
# 构建所有组件 (引导加载器、内核、用户程序)
make build
```

### 单独组件构建

```bash
# 仅构建内核 (调试模式)
make build-kernel

# 构建用户程序并转换为二进制文件
make build-user

# 构建引导加载器 (发布模式)
make build-bootloader
```

## 文件系统管理

系统使用自定义 Python 脚本创建 FAT32 文件系统镜像：

```bash
# 创建包含用户程序的文件系统
python3 create_fs.py create

# 脚本自动执行以下操作：
# - 将用户 ELF 二进制文件转换为可加载格式
# - 创建具有正确目录结构的 FAT32 镜像
# - 嵌入字体文件和其他资源
```

## 工具链要求

- **Rust Nightly**: 特定版本 `nightly-2025-06-15` (在 `rust-toolchain.toml` 中定义)
- **目标架构**: `riscv64gc-unknown-none-elf`
- **必需组件**: `rust-src`, `llvm-tools`, `rustfmt`, `clippy`
- **QEMU**: `qemu-system-riscv64` 用于 RISC-V 模拟
- **RISC-V 工具**: GNU 工具链用于调试 (`riscv64-elf-gdb`, `riscv64-unknown-elf-addr2line`)

## 代码组织模式

### 内核模块结构

- 每个子系统都有自己的模块目录 (`memory/`, `task/`, `fs/` 等)
- 模块通过 `mod.rs` 文件导出公共接口
- 硬件特定代码隔离在 `arch/` 和 `board/` 目录中
- 驱动代码遵循 VirtIO 规范模式

### 错误处理

- 广泛使用 `Result<T, E>` 处理可恢复错误
- 为内核和引导加载器实现了 panic 处理程序
- 每个子系统定义自定义错误类型 (如 `fs::FsError`)

### 内存安全

- 全程使用 `#![no_std]` 适应嵌入式环境
- 谨慎使用 `unsafe` 块并提供清晰文档
- 资源管理采用 RAII 模式 (锁、内存分配)
- 静态分析友好的代码结构

### 多核考虑

- 所有共享状态由适当的同步原语保护
- 工作窃取调度器在核心间分发任务
- 必要时使用每个 hart (核心) 的数据结构
- IPI (处理器间中断) 支持协调

## 测试和验证

系统可以通过以下方式验证：

1. 成功的启动序列，显示 RustSBI 和内核初始化
2. 用户 shell (`/bin/init`) 成功启动
3. 基本命令工作正常 (`hello_world`, `shutdown`)
4. 文件系统操作 (创建、读取、写入文件)
5. 进程管理 (fork, exec, wait)

## 关键实现细节

- **上下文切换**: `kernel/src/task/switch.S` 中的汇编实现
- **陷阱处理**: `kernel/src/trap/` 中的统一陷阱处理器
- **系统调用接口**: `kernel/src/syscall/mod.rs` 中 Linux 兼容的编号
- **设备树解析**: `bootloader/src/device_tree.rs` 中的动态硬件发现
- **虚拟内存**: 内核使用恒等映射，用户空间使用独立地址空间
