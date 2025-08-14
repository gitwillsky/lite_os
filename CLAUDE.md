# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

LiteOS is a sophisticated, Rust-based operating system designed for the RISC-V 64 architecture (`riscv64gc`). This project demonstrates modern OS development concepts including two-stage booting, multi-tasking, virtual memory management, and a comprehensive POSIX-compatible system call interface. The system features an integrated WebAssembly runtime environment and supports multiple programming languages.

## Essential Build Commands

```bash
# Complete build and run
make build              # Build all components (bootloader, kernel, user) + filesystem
make run                # Build kernel and run in QEMU (8 cores, no timeout)
make run-gui            # Run with GUI display using Cocoa
make run-with-timeout   # Run with 15-second timeout for automated testing
make run-gdb            # Run with GDB debugging support

# Individual component builds  
make build-bootloader   # Build M-Mode bootloader only
make build-kernel       # Build S-Mode kernel only  
make build-user         # Build user programs only

# Filesystem and cleanup
make create-fs          # Create FAT32 filesystem image
python3 create_fs.py create  # Alternative filesystem creation
make clean              # Clean all build artifacts

# Debugging
make gdb                # Connect GDB to running QEMU instance
make addr2line ADDR=<address>  # Convert address to source location
```

## Testing

The system includes comprehensive test suites organized as user programs:

- `tests_process` - Process management (fork, exec, wait)
- `tests_memory` - Memory management and allocation
- `tests_fs` - Filesystem operations and VFS
- `tests_signal` - Signal handling and delivery
- `tests_ipc` - Inter-process communication (pipes, sockets)
- `tests_threads` - Thread creation and management
- `tests_time` - Timer and time-related syscalls
- `tests_system` - General system functionality
- `tests_watchdog` - Watchdog timer functionality

Tests are built automatically with `make build-user` and can be executed in the shell.

## WebAssembly Support

Build WASM programs:
```bash
cd wasm_programs
./build.sh              # Builds all WASM test programs to wasm_output/
```

Execute WASM in LiteOS shell:
```bash
wasm_runtime <filename>.wasm
```

## Architecture Overview

### Multi-Component Boot Process
- **Bootloader** (M-Mode): RustSBI-compliant, handles hardware initialization and HART management
- **Kernel** (S-Mode): Full POSIX-compatible OS with advanced scheduling and VFS
- **User Programs**: Native applications plus WebAssembly runtime

### Key Subsystems

**Memory Management** (`kernel/src/memory/`):
- Multi-level page tables with process isolation
- Slab allocator for kernel objects
- Dynamic kernel heap allocation
- Frame allocator for physical memory

**Process Management** (`kernel/src/task/`):
- Multiple schedulers: FIFO, Priority-based, CFS (Completely Fair Scheduler)
- Unix-like process model with fork/exec/wait
- Multi-core HART scheduling support

**File Systems** (`kernel/src/fs/`):
- VFS (Virtual File System) layer
- FAT32 implementation with file locking
- DevFS for device files
- EXT2 support (partial)

**Device Drivers** (`kernel/src/drivers/`):
- VirtIO framework for all device types
- Block storage, GPU, input, console, networking
- Hardware abstraction layer (HAL)

**System Calls** (`kernel/src/syscall/`):
- 30+ POSIX-compatible system calls
- Process control, memory management, file I/O
- Signal handling, timer, graphics support

### Multi-Core Support
- RISC-V HART (Hardware Thread) management
- SBI HSM (Supervisor Binary Interface Hart State Management)
- Multi-core scheduling with load balancing
- Core-local interrupt handling via CLINT/ACLINT

## Development Guidelines

### Code Organization
- Keep source files under 500 lines for maintainability
- Follow POSIX standards and Unix/Linux best practices
- Design for multi-core and concurrent scenarios from the start
- Use clear, descriptive naming conventions for data structures

### Problem-Solving Approach
- Address root causes rather than applying quick fixes
- When refactoring, replace code completely rather than maintaining backward compatibility
- Leverage Rust's ownership system and type safety for memory management

### System Programming Practices
- All kernel code must be interrupt-safe and SMP-aware
- User programs should use the provided system call wrappers in `user/src/lib.rs`
- Follow the existing VirtIO driver patterns when adding new device support
- Maintain separation between M-Mode (bootloader), S-Mode (kernel), and U-Mode (user) code

### Key Files for Understanding Codebase
- `kernel/src/main.rs` - Kernel initialization and main loop
- `kernel/src/task/mod.rs` - Process management interface
- `kernel/src/syscall/mod.rs` - System call dispatch table
- `kernel/src/memory/mod.rs` - Memory management interface
- `user/src/lib.rs` - User-space system call wrappers
- `bootloader/src/main.rs` - Boot sequence and hardware setup