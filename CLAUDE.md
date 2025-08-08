# LiteOS - RISC-V 64 Operating System

## Project Overview

LiteOS is a sophisticated, Rust-based operating system designed for the RISC-V 64 architecture (`riscv64gc`). This project demonstrates modern OS development concepts including two-stage booting, multi-tasking, virtual memory management, and a comprehensive POSIX-compatible system call interface. The system features an integrated WebAssembly runtime environment and supports multiple programming languages.

## Architecture

### System Components

The LiteOS system consists of four main components:

1. **Bootloader** (`bootloader/`) - M-Mode SBI-compliant bootloader using RustSBI
2. **Kernel** (`kernel/`) - S-Mode operating system kernel with full POSIX compatibility
3. **User Programs** (`user/`) - Native user-space applications and shell
4. **WASM Runtime** (`wasm_programs/`) - WebAssembly execution environment

### Key Features

- **RISC-V 64 Support**: Native `riscv64gc` architecture implementation
- **Two-Stage Boot Process**: M-Mode bootloader → S-Mode kernel transition
- **Virtual Memory Management**: Multi-level page tables with process isolation
- **Process Management**: Unix-like process model with `fork()`, `exec()`, and `wait()`
- **File System**: VFS layer with FAT32 support and VirtIO block devices
- **Comprehensive System Calls**: 30+ implemented system calls covering all major POSIX interfaces
- **Advanced Scheduling**: Multiple scheduling algorithms (FIFO, Priority, CFS) with priority control
- **Multi-Core Hardware Support**: RISC-V HART management with up to 8 cores, SBI HSM compliance
- **IPC Mechanisms**: Pipes, signals, and file locking
- **WebAssembly Support**: WASI-compatible runtime for multi-language programs

### Common Build Commands

```bash
# Individual component builds
make build-bootloader    # Build M-Mode bootloader only
make build-kernel       # Build S-Mode kernel only
make build-user         # Build user programs only
make run-with-timeout           # Automated testing with timeout

# Filesystem management
make create-fs          # Create FAT32 filesystem image
python3 create_fs.py create  # Alternative filesystem creation
```

## Project Structure

```
lite_os/
├── bootloader/              # M-Mode bootloader (RustSBI-based)
│   ├── src/
│   │   ├── main.rs         # Boot entry point
│   │   ├── device_tree.rs  # Device tree parsing
│   │   ├── uart16550.rs    # Serial console support
│   │   ├── fast_trap/      # M-Mode trap handling
│   │   ├── aclint.rs       # Advanced Core Local Interruptor
│   │   ├── clint.rs        # Core Local Interruptor (multi-core IPI)
│   │   ├── hsm_cell/       # Hardware State Management for multi-core
│   │   ├── hart.rs         # RISC-V HART (hardware thread) management
│   │   └── console.rs      # Console support
│   └── linker.ld           # Bootloader memory layout
│
├── kernel/                  # S-Mode operating system kernel
│   ├── src/
│   │   ├── main.rs         # Kernel entry point
│   │   ├── memory/         # Virtual memory management
│   │   ├── task/           # Process/task management with advanced scheduling
│   │   ├── syscall/        # Comprehensive POSIX system call implementation
│   │   ├── fs/             # VFS layer with FAT32 and file locking support
│   │   ├── drivers/        # VirtIO device drivers (block, console, GPU, etc.)
│   │   ├── trap/           # Exception/interrupt handling
│   │   ├── ipc/            # Inter-process communication (pipes)
│   │   ├── sync/           # Synchronization primitives
│   │   ├── arch/           # Architecture-specific code (RISC-V)
│   │   └── board/          # Board support and device tree parsing
│   └── linker.ld           # Kernel memory layout
│
├── user/                    # User-space programs and libraries
│   ├── src/
│   │   ├── lib.rs          # User library (system call wrappers)
│   │   └── bin/            # Rich set of user programs
│   │       ├── init.rs          # System initialization
│   │       ├── shell.rs         # Advanced interactive shell
│   │       ├── shell_modules/   # Shell components
│   │       │   ├── builtins.rs     # Built-in commands
│   │       │   ├── completion.rs   # Tab completion
│   │       │   ├── editor.rs       # Command line editor
│   │       │   ├── executor.rs     # Command execution
│   │       │   ├── history.rs      # Command history
│   │       │   └── jobs.rs         # Job control
│   │       ├── vim.rs           # Built-in text editor
│   │       ├── top.rs           # Process monitor
│   │       ├── wasm_runtime.rs  # WebAssembly runtime
│   │       ├── wasm_runtime/    # WASM runtime components
│   │       │   ├── engine.rs       # WASM execution engine
│   │       │   ├── filesystem.rs   # WASI filesystem interface
│   │       │   ├── process.rs      # Process management
│   │       │   └── wasi.rs         # WASI implementation
│   │       ├── test.rs          # Comprehensive test suite
│   │       ├── cat.rs           # File display utility
│   │       ├── ls.rs            # Directory listing
│   │       ├── mkdir.rs         # Directory creation
│   │       ├── rm.rs            # File removal
│   │       ├── pwd.rs           # Current directory
│   │       ├── echo.rs          # Text output
│   │       ├── kill.rs          # Process signaling
│   │       └── exit.rs          # Clean shutdown
│   └── linker.ld           # User program memory layout
│
├── wasm_programs/           # WebAssembly test programs
│   ├── src/                # Rust WASM source code
│   │   ├── hello_wasm.rs   # Basic WASM hello world
│   │   ├── file_test.rs    # File I/O testing
│   │   ├── math_test.rs    # Mathematical operations
│   │   └── wasi_test.rs    # WASI interface testing
│   ├── build.sh            # WASM build script
│   └── wasm_output/        # Compiled .wasm files
│
├── Makefile                # Main build system
├── Cargo.toml              # Workspace configuration
├── rust-toolchain.toml     # Rust toolchain specification
├── create_fs.py            # Filesystem creation utility
├── virt-riscv64.dtb        # Device tree binary
├── virt-riscv64.dts        # Device tree source
├── README.md               # User documentation (Chinese)
└── TODO.md                 # Development roadmap (Chinese)
```

记住：

1. 你是一位专业的 Rust 程序员和操作系统开发者
2. 合理组织源代码，每个源文件不超过 500 行
3. 你的代码实现遵循 POSIX 规范、Unix 或 Linux 最佳实践
4. 充分考虑并发、多核场景
5. 遇到问题不要简化实现，直面问题，寻找根因，彻底解决
6. 重构或者修改时，直接替换原有代码，不用考虑向前兼容
7. 保持数据结构简洁清晰，命名规范合理