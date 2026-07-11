# Phase 29：唯一默认 BusyBox userspace

## 结论

`make build`、`make run`、`make run-gdb` 和 SMP boot gate 现在全部消费同一个 `fs.img`：固定 musl v1.2.6 静态链接的上游 BusyBox 1.37.0。rootfs 只有一个 ELF inode，`init/ash/sh` 和基础 applet 均是 hardlink。不再存在 Rust init、自有 `_start`/syscall runtime、`build-user`、默认 Rust init 镜像或两套 boot marker。

## 构建边界

1. `build-musl` 从固定官方 tarball 构造静态 sysroot 并完成 consumer ELF 检查。
2. `build-rootfs` 从固定 BusyBox tarball 和唯一 config 生成 `fs.img`；`create_fs.py` 只是要求显式 `--init` 的底层 ext2 primitive。
3. `build` 组合 bootloader、kernel 与 rootfs；默认路径不运行 QEMU，运行观察只在 `run` 或 verify gate。

BusyBox/musl 源码、sysroot 和 ELF 都在 `target/` 下，不入库。`user/` 只保留可审查的 BusyBox config/inittab 与固定 musl ABI consumer 源码，不是第二个 runtime。

## 验收

- artifact gate 检查 bootloader、kernel 和实际 BusyBox ELF，不再检查已删除 init。
- SMP gate 用默认 `fs.img` 完成 1/3/8-hart 冷启动并观察 BusyBox init。
- BusyBox gate 继续覆盖 UART shell、pipeline、重定向、后台 wait、Ctrl-C 和跨冷启动持久化。
- 架构围栏明确禁止 Rust user source/Cargo/linker、旧 build target 与旧 init artifact，并要求默认 Makefile 经固定 BusyBox builder 产生 `fs.img`；恢复双轨会直接使围栏失败。
