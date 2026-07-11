# Phase 23：UART IRQ console 与真实 ash 输入

> 本文保留 Phase 23 的历史边界；Phase 24 已补 `rt_sigtimedwait/SIGCHLD`，Phase 25 已把 raw console 原地升级为 Terminal/session/termios/foreground Ctrl-C 模型。

## 目标

让固定 BusyBox rootfs 不只打印 init banner，而是从 QEMU UART 接收真实输入、阻塞/唤醒 console reader，并由 ash 执行命令产生不可由输入原文伪造的输出标记。

## 设备与等待模型

DTB `serial/uart` 节点同时提供 MMIO range 与 IRQ 10；BoardInfo 保存两者，kernel page table 只映射该 DTB range。UART driver 唯一拥有 16550 endpoint 与预分配 1024-byte RX ring：

1. hardirq volatile 读取 LSR/RBR 直到 FIFO 清空；ring 满时继续读设备并丢弃超额 byte，避免 level IRQ 永久重入；hardirq 不分配、不调度；
2. hardirq 发布当前 hart 的 console softirq；统一 per-hart bitset 一次消费 timer/console work，避免分别 clear SSIP 丢失另一类 work；
3. console `read` 先取 ring；为空时在 IndexedWaitQueue owner lock 内复查 `input_ready`，再发布唯一 Console membership；signal cancellation、IRQ wake 竞争时只有先移除 registration 的一方生效；
4. deferred consumer 移除 Console membership 并走原 `Blocking -> WakePending -> Ready` 协议，不扫描 process graph。

PID 1 的 fd 0/1/2 仍是同一 console OFD：read 走 UART ring，write/writev 走 SBI DBCN，不存在 console syscall 旁路。当前没有 termios/canonical/echo/foreground process group，因此只声明 raw byte stream。

## BusyBox rootfs 与 writev

gate 校验 BusyBox 1.37.0 tarball/config 后构造 ext2：`/bin/init` 是唯一 BusyBox inode，`ash/sh/busybox` 与基础 applet 使用正确 link count 的 hardlink，`/etc/inittab` 是唯一 `askfirst` shell 配置。CI 等待 askfirst prompt 后注入：

```sh
echo LITEOS_BUSYBOX_SHELL_$((6*7))
```

输入字节不包含最终 marker `LITEOS_BUSYBOX_SHELL_42`，因此 marker 证明 ash 完成解析、算术扩展与 builtin 执行。首次 gate 进一步暴露 `writev(66)`；实现一次导入全部 RV64 iovec、`IOV_MAX=1024`、`SSIZE_MAX` 总长和 partial-write 后，交互 gate 通过。

## 精确边界

- 已成立：DTB UART/PLIC、IRQ RX、无丢失 read/enqueue、interruptible console wait、BusyBox init fork/exec、ash 输入与 builtin output。
- 未成立：`ioctl/termios`、canonical/echo、session/process group、foreground TTY、Ctrl-C、`rt_sigtimedwait`、vfork、pipe/redirection/background job。
- 未知 syscall 只返回 `ENOSYS` 并使用 debug 诊断；用户请求不再污染 `[ERROR]` fatal channel，缺口仍由 syscall 矩阵与本阶段文档记录。

## 验证

`make verify` 覆盖架构 interface、Clippy、三组件构建、1/3/8 hart Rust init、musl pthread/signal/cwd gate，以及 BusyBox 单 hart UART 交互 gate。该阶段不把默认 rootfs 切到 BusyBox；切换只能在后续 pipe/TTY/job-control 与持久化 gate 都成立后一次完成并删除 Rust init 路径。
