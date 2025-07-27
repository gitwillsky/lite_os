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
- **POSIX System Calls**: 82 implemented system calls covering all major POSIX interfaces
- **Advanced Scheduling**: Four scheduling algorithms (FIFO, Priority, Round-Robin, CFS)
- **IPC Mechanisms**: Pipes, signals, and file locking
- **WebAssembly Support**: WASI-compatible runtime for multi-language programs

## Quick Start

### Prerequisites

- **Rust Nightly Toolchain**: Specifically `nightly-2025-06-15` (defined in `rust-toolchain.toml`)
- **QEMU**: With RISC-V 64 support (`qemu-system-riscv64`)
- **RISC-V GNU Toolchain**: For debugging tools (`riscv64-elf-gdb`, `llvm-addr2line`)
- **Build Tools**: `dosfstools` for filesystem creation

#### macOS Installation
```bash
# Install QEMU
brew install qemu

# Install RISC-V toolchain
brew tap riscv/riscv
brew install riscv-gnu-toolchain

# Install filesystem tools
brew install dosfstools
```

#### Linux Installation
```bash
# Install QEMU (Ubuntu/Debian)
sudo apt-get install qemu-system-misc

# Install RISC-V toolchain
sudo apt-get install binutils-riscv64-unknown-elf

# Install filesystem tools
sudo apt-get install dosfstools
```

### Build and Run

```bash
# Clone and enter directory
git clone <repository-url>
cd lite_os

# Full build (all components + filesystem)
make build

# Quick kernel-only build
make build-kernel

# Run in QEMU
make run

# Run with automatic timeout (useful for CI)
make run-with-timeout

# Debug with GDB
make run-gdb    # Terminal 1
make gdb        # Terminal 2

# Clean build artifacts
make clean
```

### Using the System

After booting, you'll see a shell prompt (`$`). Available commands include:

```bash
# Native programs
$ test           # Run test suite
$ exit           # Clean shutdown

# WASM programs (with runtime)
$ wasm_runtime hello_wasm.wasm
$ wasm_runtime file_test.wasm
```

## Development Workflows

### Common Build Commands

```bash
# Individual component builds
make build-bootloader    # Build M-Mode bootloader only
make build-kernel       # Build S-Mode kernel only
make build-user         # Build user programs only

# Filesystem management
make create-fs          # Create FAT32 filesystem image
python3 create_fs.py create  # Alternative filesystem creation

# WASM development
cd wasm_programs && ./build.sh  # Build WASM test programs
```

### Debugging

```bash
# Kernel debugging with GDB
make run-gdb                    # Start QEMU with GDB server
make gdb                        # Connect GDB to kernel

# Address-to-line debugging
make addr2line ADDR=0x12345678  # Convert address to source location

# QEMU testing
make run-with-timeout           # Automated testing with timeout
```

### File System Development

The system uses a FAT32 filesystem image (`fs.img`) created by `create_fs.py`:

```bash
# Create new filesystem
python3 create_fs.py create

# Add files to filesystem (requires mounting)
# Copy user binaries and WASM files to mounted filesystem
```

## Project Structure

```
lite_os/
├── bootloader/              # M-Mode bootloader (RustSBI-based)
│   ├── src/
│   │   ├── main.rs         # Boot entry point
│   │   ├── device_tree.rs  # Device tree parsing
│   │   ├── uart16550.rs    # Serial console support
│   │   └── fast_trap/      # M-Mode trap handling
│   └── linker.ld           # Bootloader memory layout
│
├── kernel/                  # S-Mode operating system kernel
│   ├── src/
│   │   ├── main.rs         # Kernel entry point
│   │   ├── memory/         # Virtual memory management
│   │   ├── task/           # Process/task management
│   │   ├── syscall/        # POSIX system call implementation
│   │   ├── fs/             # VFS and FAT32 filesystem
│   │   ├── drivers/        # VirtIO device drivers
│   │   ├── trap/           # Exception/interrupt handling
│   │   ├── ipc/            # Inter-process communication
│   │   └── sync/           # Synchronization primitives
│   └── linker.ld           # Kernel memory layout
│
├── user/                    # User-space programs
│   ├── src/
│   │   ├── lib.rs          # User library (system call wrappers)
│   │   └── bin/            # User programs
│   │       ├── user_shell.rs    # Interactive shell
│   │       ├── wasm_runtime.rs  # WebAssembly runtime
│   │       └── test.rs          # Test programs
│   └── linker.ld           # User program memory layout
│
├── wasm_programs/           # WebAssembly test programs
│   ├── src/                # Rust WASM source code
│   ├── build.sh            # WASM build script
│   └── wasm_output/        # Compiled .wasm files
│
├── Makefile                # Main build system
├── Cargo.toml              # Workspace configuration
├── rust-toolchain.toml     # Rust toolchain specification
├── create_fs.py            # Filesystem creation utility
├── README.md               # User documentation (Chinese)
└── TODO.md                 # Development roadmap (Chinese)
```

## Key System Interfaces

### System Calls

LiteOS implements 82 POSIX-compatible system calls organized by category:

**File Operations:**
- `open`, `close`, `read`, `write`, `lseek`
- `mkdir`, `rmdir`, `stat`, `chmod`, `chown`
- File descriptor management and locking (`flock`)

**Process Management:**
- `fork`, `exec`, `execve`, `wait`, `wait_pid`
- `getpid`, `getppid`, `exit`
- Priority and scheduler control

**Memory Management:**
- `brk`, `mmap`, `munmap`
- Dynamic memory allocation

**Inter-Process Communication:**
- `pipe`, `dup`, `dup2`
- Signal handling (`signal`, `sigaction`, `kill`)

**Security:**
- User/group management (`getuid`, `setuid`, `getgid`, `setgid`)
- Permission controls

### Device Interface

**VirtIO Devices:**
- **Block Device**: Storage via `virtio-blk-device`
- **Console**: Serial I/O via VirtIO console (optional)
- **Random**: Hardware random number generation
- **Network**: Ethernet via `virtio-net-device`

**Hardware Abstraction Layer (HAL):**
- Device enumeration and lifecycle management
- Interrupt controller integration
- Memory-mapped I/O abstractions

## WASM Runtime Environment

### Supported Languages

LiteOS supports WebAssembly programs compiled from multiple languages:

- **Rust**: Native integration with `wasm32-wasip1` target
- **C/C++**: Via clang/wasi-sdk toolchain
- **Go**: TinyGo with WASI support
- **Other**: Any language with WASI compilation support

### WASI Interface Mapping

The WASM runtime maps WASI standard interfaces to LiteOS system calls:

```rust
// File operations
wasi::fd_read()     → SYSCALL_READ (63)
wasi::fd_write()    → SYSCALL_WRITE (64)
wasi::path_open()   → SYSCALL_OPEN (56)

// Process control
wasi::proc_exit()   → SYSCALL_EXIT (93)
wasi::sched_yield() → SYSCALL_YIELD (124)

// Environment
wasi::args_get()    → argv from execve
wasi::environ_get() → envp from execve
```

### Building WASM Programs

```bash
# Build all WASM test programs
cd wasm_programs
./build.sh

# Individual WASM builds
cargo build --release --target wasm32-wasip1 --bin hello_wasm

# Run WASM programs in LiteOS
$ wasm_runtime hello_wasm.wasm
$ wasm_runtime file_test.wasm arg1 arg2
```

## Memory Layout

### Kernel Memory Map
- **Kernel Code**: Loaded at high memory addresses
- **Page Tables**: Multi-level page table structure
- **Heap**: Dynamic kernel memory allocation
- **Device MMIO**: Memory-mapped device regions

### User Memory Map
- **Code Segment**: ELF program sections
- **Data/BSS**: Static data and uninitialized memory
- **Heap**: User heap via `brk`/`mmap`
- **Stack**: User-mode stack

## Testing and Validation

### Automated Testing
```bash
# Full system test with timeout
make run-with-timeout

# Kernel unit tests (if available)
cd kernel && cargo test

# WASM program tests
cd wasm_programs && ./build.sh
```

### Manual Testing
```bash
# Boot to shell and run tests
make run
$ test           # Run built-in test suite
$ wasm_runtime hello_wasm.wasm  # Test WASM execution
```

## Troubleshooting

### Common Issues

**Build Problems:**
- Ensure correct Rust nightly version (`nightly-2025-06-15`)
- Verify RISC-V target installation: `rustup target add riscv64gc-unknown-none-elf`
- Check QEMU installation and RISC-V support

**Runtime Issues:**
- **Boot failure**: Check bootloader/kernel compatibility
- **No filesystem**: Ensure `fs.img` exists (`make create-fs`)
- **Device errors**: Verify QEMU device configuration in Makefile

**WASM Issues:**
- **Missing target**: `rustup target add wasm32-wasip1`
- **Runtime errors**: Check WASI interface compatibility
- **Performance**: Monitor memory usage and execution time

### Debug Commands

```bash
# Kernel debugging
make run-gdb && make gdb

# Memory analysis
make addr2line ADDR=<address>

# QEMU console debugging
# Use Ctrl+A, C to access QEMU monitor
# Use Ctrl+A, X to exit QEMU
```

## Development Notes

### Code Style
- Standard Rust formatting with `rustfmt`
- Comprehensive error handling
- Extensive documentation comments
- Safety-first approach with minimal `unsafe` code

### Contributing Areas
1. **WASM Runtime Enhancement**: Improve WASI compatibility and performance
2. **Device Drivers**: Add support for additional VirtIO devices
3. **Network Stack**: Implement TCP/IP networking
4. **File Systems**: Add support for additional filesystem types
5. **Scheduler Improvements**: Enhance CFS or add new scheduling algorithms

### Architecture Goals
- **Education**: Clear, well-documented code for learning OS concepts
- **Performance**: Efficient implementation suitable for embedded/IoT use cases
- **Compatibility**: Strong POSIX compliance for application portability
- **Innovation**: Modern language (Rust) applied to systems programming

This LiteOS implementation represents a significant achievement in modern operating system development, combining educational clarity with production-ready features and innovative WebAssembly integration.

可用的二进制命令：
ls ~/.cargo/bin
cargo          cargo-objcopy  cargo-watch    rust-cov       rust-nm        rust-strip
cargo-clippy   cargo-objdump  clippy-driver  rust-gdb       rust-objcopy   rustc
cargo-cov      cargo-profdata hi             rust-gdbgui    rust-objdump   rustdoc
cargo-fmt      cargo-readobj  rls            rust-ld        rust-profdata  rustfmt
cargo-miri     cargo-size     rust-analyzer  rust-lld       rust-readobj   rustup
cargo-nm       cargo-strip    rust-ar        rust-lldb      rust-size