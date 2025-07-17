# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

LiteOS is a Rust-based operating system for RISC-V 64-bit architecture with a three-tier structure:

1. **Bootloader** (M-Mode): RustSBI implementation handling hardware initialization
2. **Kernel** (S-Mode): Core OS with memory management, process control, filesystem, and drivers
3. **User Space**: Applications and interactive shell

你不要自己执行 make 命令，由我来执行

## Architecture

### Core Components

- **Memory Management**: Multi-level page tables, per-process virtual address spaces, buddy allocator
- **Process Model**: Unix-like fork/exec with simple scheduler, PID management
- **File System**: VFS layer with FAT32 support, VirtIO block device backend
- **System Calls**: `fork`, `exec`, `wait`, `read`, `write`, `open`, `mkdir`, `listdir`, etc.
- **Device Drivers**: VirtIO framework, MMIO interface, interrupt handling

### Project Structure

- `bootloader/`: M-Mode SBI implementation with hardware initialization
- `kernel/src/`: S-Mode kernel with subsystems:
  - `memory/`: Virtual memory, physical frames, heap management
  - `task/`: Process control blocks, context switching, scheduling
  - `fs/`: VFS, FAT32 filesystem, inode interface
  - `drivers/`: VirtIO block devices, device manager
  - `syscall/`: System call implementations
- `user/`: User space applications and shell
- `target/`: Build outputs for RISC-V target

### Key Files

- `Makefile`: Build automation with QEMU integration
- `rust-toolchain.toml`: Nightly Rust 2025-06-15 specification
- `virt-riscv64.dts`: Device tree for QEMU virt machine
- `create_fs.py`: Python script to generate FAT32 filesystem with test files
- `fs.img`: 64MB FAT32 filesystem image mounted as VirtIO block device

## Development Notes

### Target Architecture

- Platform: `riscv64gc-unknown-none-elf`
- Emulation: QEMU virt machine with 2GB RAM
- Boot: Two-stage (M-Mode bootloader → S-Mode kernel)

### Filesystem Development

- FAT32 implementation supports read/write operations
- Directory operations (mkdir, listdir) recently implemented
- VirtIO block device provides storage backend
- Test files automatically created by `create_fs.py`

### Debugging

- GDB support with RISC-V toolchain
- Comprehensive logging system with configurable levels
- QEMU monitor available for hardware debugging

### Common Issues

- Ensure `fs.img` exists before running (use `make create-fs`)
- FAT32 filesystem must be properly initialized for file operations
- QEMU requires RISC-V system emulation support
