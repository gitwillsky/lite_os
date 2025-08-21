# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is a RISC-V 64-bit operating system kernel written in Rust, featuring a two-stage boot process with an M-Mode bootloader and S-Mode kernel. The OS supports multi-tasking, virtual memory management, system calls, and user-space programs.

## Build Commands

### Basic Build and Run

- `make build` - Build all components (bootloader, kernel, user programs, filesystem)
- `make run` - Build and run the OS in QEMU (8 cores, no GUI)
- `make run-gui` - Build and run with GUI support (Cocoa display on macOS)
- `make clean` - Clean all build artifacts

### Individual Components

- `make build-bootloader` - Build only the M-Mode bootloader
- `make build-kernel` - Build only the S-Mode kernel (debug mode)
- `make build-user` - Build all user programs and convert to binaries
- `make create-fs` - Create filesystem image using Python script

### Debugging

- `make run-gdb` - Start QEMU with GDB server (paused at first instruction)
- `make gdb` - Connect GDB to running QEMU instance
- `make addr2line ADDR=<address>` - Convert address to source location (requires hex address)

### Testing

- `make run-with-timeout` - Run with 15-second timeout (kills QEMU automatically)

## Architecture

### Three-Component Structure

1. **Bootloader** (`bootloader/`) - M-Mode SBI implementation using RustSBI
2. **Kernel** (`kernel/`) - S-Mode operating system kernel
3. **User Programs** (`user/`) - User-space applications and libraries

### Key Kernel Modules

- `task/` - Process/thread management with CFS, FIFO, and priority schedulers
  - `processor.rs` - Per-CPU task execution and scheduling
  - `scheduler/` - Multiple scheduling algorithms (CFS, FIFO, Priority)
  - `task_manager.rs` - Global task management and multi-core load balancing
- `memory/` - Virtual memory management with multi-level page tables
  - `frame_allocator.rs` - Buddy system for physical memory allocation
  - `slab_allocator.rs` - SLAB allocator for kernel objects
  - `mm.rs` - Memory set and map area management
  - `page_table.rs` - SV39 page table implementation
- `syscall/` - System call interface (200+ syscalls including POSIX-like calls)
  - Organized by category: fs, process, signal, timer, memory, graphics, etc.
- `fs/` - Virtual filesystem with FAT32, EXT2, DevFS support
  - `vfs.rs` - Virtual filesystem layer with mount support
  - `fat32/` - FAT32 filesystem implementation
  - `ext2/` - EXT2 filesystem implementation
  - `devfs/` - Device filesystem (/dev)
- `drivers/` - VirtIO device drivers (block, GPU, input, console)
- `signal/` - POSIX-style signal handling
- `trap/` - Interrupt and exception handling
- `ipc/` - Inter-process communication (pipes, Unix domain sockets)

### Memory Layout

- Kernel uses identity mapping for physical memory access
- Per-CPU kernel stacks with guard pages
- Separate user address spaces with demand paging
- SLAB allocator for kernel objects, buddy allocator for frames

### Multi-Core Support

- SMP support for up to 8 cores (configurable in QEMU)
- Per-CPU task scheduling with load balancing
- Lock-free Per-CPU design for improved performance

## User Programs

User programs are located in `user/src/bin/` and include:

- `init.rs` - Init process (PID 1)
- `shell.rs` - Interactive shell with command parsing and job control
- Standard utilities: `ls`, `cat`, `mkdir`, `rm`, `pwd`, `echo`, `kill`, `top`, `exit`
- Test suites: `tests_*.rs` for various subsystems
  - `tests_fs.rs` - File system operations
  - `tests_process.rs` - Process management
  - `tests_memory.rs` - Memory management
  - `tests_signal.rs` - Signal handling
  - `tests_threads.rs` - Multi-threading
  - `tests_time.rs` - Time functions
  - `tests_watchdog.rs` - Watchdog timer
  - `tests_ipc.rs` - Inter-process communication
  - `tests_system.rs` - System-wide tests
- GUI applications: `gui_demo.rs`, `litewm.rs` (window manager), `webwm.rs`
- Text editor: `vim.rs`

## System Calls

The kernel implements 200+ system calls organized by category:

- Process management: `fork`, `exec`, `wait`, `exit`, `getpid`
- File I/O: `open`, `read`, `write`, `close`, `lseek`, `stat`
- Memory management: `mmap`, `munmap`, `brk`, `sbrk`
- Signal handling: `kill`, `signal`, `sigaction`, `sigprocmask`
- Time functions: `gettimeofday`, `nanosleep`, various time getters
- Graphics: GUI context management and framebuffer access
- Watchdog: Hardware watchdog timer control

## Development Notes

### Toolchain Requirements

- Rust nightly toolchain (required for no_std features)
- QEMU with RISC-V support (`qemu-system-riscv64`)
- RISC-V GNU toolchain for debugging (`riscv64-elf-gdb`, `riscv64-unknown-elf-addr2line`)
- Python 3 (for filesystem image creation)
- Optional: E2fsprogs (for EXT2 filesystem support on macOS)

### Key Files to Understand

- `kernel/src/main.rs:35` - Kernel entry point (`kmain`)
- `kernel/src/syscall/mod.rs:126` - System call dispatcher
- `kernel/src/task/mod.rs:32` - Task subsystem initialization
- `kernel/src/memory/mod.rs:44` - Memory management initialization
- `bootloader/src/main.rs:50` - Bootloader main function

### Debugging Tips

- Use `make addr2line ADDR=<hex_address>` to resolve panic addresses
- Enable specific logging with environment variables in kernel build
- GDB debugging requires two terminals (one for QEMU, one for GDB)
- Check `fs.img` filesystem contents with `python3 create_fs.py` commands
- Default log level is set in `kernel/src/config/mod.rs`
- Disable specific module logging with `log::disable_module()` in `kernel/src/main.rs`
- For timeout testing, use `make run-with-timeout` (15-second auto-kill)

### Common Patterns

- Error handling uses custom error types, not `std::error`
- Kernel uses `spin` crate for synchronization primitives (Mutex, Once)
- Memory allocation uses custom allocators (buddy + SLAB)
- Device drivers follow VirtIO specification
- User programs link against custom `user` library crate
- Per-CPU design for scalability (kernel stacks, task queues)
- Lazy initialization with `spin::Once` for global resources
- File paths are resolved relative to current working directory
- System calls use usize arrays for arguments, isize for returns

## Testing

Run individual test suites in the OS:

- File system tests: run `tests_fs` in shell
- Process tests: run `tests_process` in shell
- Memory tests: run `tests_memory` in shell
- Signal tests: run `tests_signal` in shell
- Thread tests: run `tests_threads` in shell
- Time tests: run `tests_time` in shell
- Watchdog tests: run `tests_watchdog` in shell
- IPC tests: run `tests_ipc` in shell
- System tests: run `tests_system` in shell

For automated testing with timeout: `make run-with-timeout`

## Filesystem Image Creation

The `create_fs.py` script manages filesystem images:

- `python3 create_fs.py create` - Create new fs.img with user binaries
- Supports both FAT32 and EXT2 filesystem formats
- Automatically copies user binaries from `target/riscv64gc-unknown-none-elf/release/*.bin`
- Cleans up macOS AppleDouble files (._*) automatically

## QEMU Configuration

- Default: 8 cores, no GUI (`make run`)
- GUI mode: Adds Cocoa display on macOS (`make run-gui`)
- Memory: 1GB in GUI mode
- Devices: VirtIO block, GPU, keyboard, mouse, network, RNG
- Network: Port forwarding localhost:5555 -> guest:5555
