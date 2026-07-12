# Phase 41：系统身份可观测性竖切

## 目标

以 system module 的唯一 immutable identity 实现 Linux `uname(160)`，并让固定动态 BusyBox `uname/arch` 成为真实 consumer。禁止在 syscall 复制字段、引入无 mutation ABI 的可变 hostname 状态，或声称 UTS namespace。

## 固定规范

- Linux `v7.1` commit `8cd9520d35a6c38db6567e97dd93b1f11f185dc6`：`include/uapi/linux/utsname.h`、`include/uapi/asm-generic/unistd.h` 与 `kernel/sys.c::newuname`。
- musl `v1.2.6` commit `9fa28ece75d8a2191de7c5bb53bed224c5947417`：`src/misc/uname.c` 与 `sys/utsname.h`。
- BusyBox `1.37.0`：`coreutils/uname.c`；`arch` 与 `uname -m` 共用同一 applet implementation。

`new_utsname` 依次包含 sysname、nodename、release、version、machine、domainname 六个 65-byte NUL-terminated field，总长度 390 bytes。

## 唯一状态路径与验收

system module 返回 `LiteOS`、`liteos`、Cargo package version、`#1 SMP PREEMPT`、`riscv64`、`(none)`；syscall 只做零初始化、定长编码和 user-copy。BusyBox gate 分别验证 `uname -s/-n/-m/-o` 与 `arch`，并要求 `LITEOS_SYSTEM_IDENTITY_42`。当前没有 sethostname/setdomainname 或 UTS namespace，字段不可变。
