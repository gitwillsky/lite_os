# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Quick commands

- Build everything: make build
- Clean: make clean
- Build individual parts:
  - Kernel: make build-kernel
  - User programs: make build-user
  - Bootloader: make build-bootloader
- Resolve a kernel backtrace address: make addr2line ADDR=0xXXXXXXXXXXXX

注意：我不允许你执行 make run* 之类的命令

## High-level architecture

### Three components

1) Bootloader (bootloader/) — RustSBI-based M-mode loader that sets up machine state and enters the S-mode kernel. It is its own Cargo project (excluded from the workspace).
2) Kernel (kernel/) — S-mode OS kernel (no_std) targeting riscv64gc-unknown-none-elf. Default member of the workspace.
3) User (user/) — no_std userland crate producing multiple binaries (user/src/bin/*) that run on the kernel.

### Kernel big picture

- Entry and init: kernel/src/main.rs contains kmain; low-level entry in kernel/src/entry.rs. Platform specifics under kernel/src/arch/.
- Syscalls: kernel/src/syscall/mod.rs dispatches 200+ calls grouped by domain (fs, process, signal, timer, memory, graphics, watchdog, IPC).
- Tasks and scheduling: kernel/src/task/ implements processes/threads with per-CPU execution; schedulers live in kernel/src/task/scheduler/ (CFS, FIFO, Priority). Task management and load balancing are in kernel/src/task/task_manager.rs and processor.rs.
- Memory management: SV39 page tables and address translation in kernel/src/memory/page_table.rs; address types in address.rs; virtual memory areas in mm.rs; frame allocation via buddy allocator (frame_allocator.rs); kernel object allocation via SLAB (slab_allocator.rs); per-CPU stacks and guard pages.
- Filesystems and VFS: kernel/src/fs/ provides a VFS layer (vfs.rs) with FAT32 (fat32.rs), EXT2 (ext2.rs), and DevFS (devfs.rs). Common inode and flock support under fs/.
- Drivers and devices: VirtIO stack under kernel/src/drivers/ (blk, gpu, input, console, queue, hal). Framebuffer and GPU support back GUI syscalls. Device/interrupt/memory abstraction in drivers/hal/.
- Traps, timers, signals: kernel/src/trap/ for interrupts/exceptions/softirq; timers in timer.rs and goldfish_rtc.rs; POSIX-like signal handling in kernel/src/signal/.
- IPC: pipes and Unix-domain sockets in kernel/src/ipc/.

### Graphics/GUI

- Kernel exposes GUI/Framebuffer syscalls (kernel/src/syscall/graphics.rs) and rect-based flush APIs.
- Userland has a minimal 2D stack in user/src/gfx.rs and a tiny GUI toolkit (user/src/litegui.rs).
- Window managers: user/src/bin/litewm.rs and user/src/bin/webwm.rs; init.rs often starts a GUI session by spawning the WM.

### Userland runtime and apps

- The user crate (user/) is no_std with a thin libc-like syscall veneer in user/src/syscall.rs and program entry in user/src/lib.rs.
- CLI utilities (ls, cat, mkdir, rm, pwd, echo, kill, top, exit) and shell (user/src/bin/shell.rs) live under user/src/bin/.
- Web rendering engine (WebCore) under user/src/webcore/ implements HTML/CSS parsing, style, layout, and painting; see user/src/webcore/README.md for details. Demo apps: css_test.rs, text_test.rs, webwm.rs.

## Build/toolchain notes

- Workspace root Cargo.toml includes kernel and user; bootloader is a separate crate (exclude) with its own .cargo/config.toml and linker script.
- All crates target riscv64gc-unknown-none-elf via per-crate .cargo/config.toml; linker scripts live under kernel/linker.ld and user/linker.ld.
- QEMU is configured for an 8-core virt machine; GUI mode adds Cocoa display and maps devices (VirtIO block/GPU/input/net/RNG). Network forwards host 5555 to guest 5555.

 ls ~/.cargo/bin
cargo          cargo-readobj  rust-cov       rust-profdata
cargo-clippy   cargo-size     rust-gdb       rust-readobj
cargo-cov      cargo-strip    rust-gdbgui    rust-size
cargo-fmt      cargo-watch    rust-ld        rust-strip
cargo-miri     clippy-driver  rust-lld       rustc
cargo-nm       hi             rust-lldb      rustdoc
cargo-objcopy  rls            rust-nm        rustfmt
cargo-objdump  rust-analyzer  rust-objcopy   rustup
cargo-profdata rust-ar        rust-objdump

每一次操作文件之前，都进行深度思考，不要吝啬使用自己的智能，人类发明你，不是为了让你偷懒。ultrathink 而是为了创造伟大的产品，推进人类文明向更高水平发展。 ultrathink ultrathink ultrathink ultrathink
