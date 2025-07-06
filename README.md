# little os

一个使用 Rust 语言编写的、支持 RISC-V 64 架构的简单操作系统内核。

## ✨ 功能特性

- **RISC-V 64 支持**: 专为 `riscv64gc` 架构设计。
- **两级启动加载**: 包含一个 M-Mode 的 `bootloader` 和一个 S-Mode 的 `kernel`。
- **多任务处理**: 实现了基于 `fork` + `exec` 的进程模型和简单的任务调度。
- **系统调用**: 支持 `read`, `write`, `fork`, `exec`, `wait`, `yield`, `shutdown` 等基础系统调用。
- **虚拟内存管理**: 实现了多级页表，为每个进程提供独立的虚拟地址空间。
- **用户态程序**: 支持在用户态运行独立的应用程序。
- **简单的 Shell**: 内置一个 `user_shell`，可以交互式地执行其他程序。

## 🛠️ 环境要求

在开始之前，请确保你已经安装了以下工具：

- **Rust Nightly 工具链**: 可以通过 `rustup` 安装。
- **QEMU (riscv64 支持)**: 用于运行和模拟操作系统。
- **RISC-V GNU 工具链**: 提供 `addr2line` 等调试工具。

在 macOS 上，可以通过 Homebrew 安装依赖：

```bash
# 安装 QEMU
brew install qemu

# 安装 RISC-V 调试工具链
brew tap riscv/riscv
brew install riscv-gnu-toolchain
```

在 Debian/Ubuntu 上：

```bash
# 安装 QEMU
sudo apt-get install qemu-system-misc

# 安装 RISC-V 调试工具链
sudo apt-get install binutils-riscv64-unknown-elf
```

## 🚀 构建与运行

1.  **克隆仓库**

    ```bash
    git clone <your-repo-url>
    cd little-os
    ```

2.  **构建项目**
    项目包含 bootloader, kernel 和 user 三个部分。`Makefile` 会自动处理所有构建步骤。

    ```bash
    make build
    ```

3.  **在 QEMU 中运行**
    此命令会构建整个项目并在 QEMU 中启动操作系统。

    ```bash
    make run
    ```

    启动后，你将看到 `$` 提示符，表示你已经进入了 `user_shell`。

4.  **调试 (GDB)**
    如果你需要使用 GDB 调试内核：

    - 在第一个终端窗口中，启动带调试服务器的 QEMU：
      ```bash
      make run-gdb
      ```
      QEMU 会暂停在启动的第一条指令。
    - 在第二个终端窗口中，启动 GDB 并连接：
      ```bash
      make gdb
      ```

5.  **清理构建产物**
    ```bash
    make clean
    ```

## ⌨️ 使用

系统启动后会进入一个简单的 shell。你可以输入以下命令来执行对应的程序：

- `hello_world`: 打印 "Hello from user program!"。
- `shutdown`: 安全地关闭 QEMU 模拟器。
- 其他你添加到 `user/src/bin` 目录下的程序。
