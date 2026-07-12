# Phase 34：BusyBox serial-console probe

## 根因

本机可用的交叉编译器 target 是 `riscv64-unknown-elf`。musl specs 负责 libc include/link 路径，但不会把 bare-metal compiler 伪装成完整 Linux compiler，因此没有预定义 `__linux__`，sysroot 也没有 Linux kernel UAPI headers。

BusyBox 1.37.0 在未启用 `FEATURE_INIT_SYSLOG` 时默认令 `log_console=/dev/tty5`。其上游 `init.c` 只有在 `VT_OPENQRY` 可见时才探测 stdin：Linux virtual terminal 返回可用 VT，UART/serial terminal 返回 `ENOTTY` 并清除独立 tty5 logger。旧构建看不到 `VT_OPENQRY`，探测被预处理器裁掉，devfs 又正确地不提供虚假的 `/dev/tty5`，因而每条 init log 都重复报告 open 失败。

## 单一路径修复

BusyBox build recipe 从固定 Linux `v7.1` revision `8cd9520d35a6c38db6567e97dd93b1f11f185dc6` 显式公布当前 consumer 所需的 `VT_OPENQRY=0x5600`，其权威定义来自 [`include/uapi/linux/vt.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/linux/vt.h)。revision 与 CPP flag 一起进入 binary fingerprint。

不全局定义 `__linux__`：那会打开 BusyBox 的完整 Linux 条件编译面，但当前 sysroot 没有对应 kernel UAPI headers，也不代表 LiteOS 已实现完整 Linux ABI。不新增 `/dev/tty5`、不修改 inittab、不启用无 syslogd 后端的 init syslog，也不 patch BusyBox 源码。

kernel 的唯一 TTY ioctl dispatcher 对 UART 不支持的 VT request 返回标准 `ENOTTY`。BusyBox 因此执行原生 serial-console 分支，保留继承的 stdin/stdout/stderr 与 `/dev/console`，不再尝试 tty5。

## 围栏

- 修复前的当前镜像可稳定触发 `init: can't log to /dev/tty5`。
- BusyBox 1-hart 与 8-hart 两次冷启动统一禁止该 marker、`Invalid argument` 与已知 unsupported syscall marker。
- 修复后 `init + ash` UART、pipeline、重定向、持久化和 foreground Ctrl-C gate 全部通过。
