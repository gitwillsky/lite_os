# Phase 21：signal disposition 驱动的 syscall restart

## 目标

在 Phase 20 已成立的 wait cancellation 上增加单一 syscall replay 协议。kernel 内部可以表达“等待实际 signal disposition 决定”，但 U-mode 只能观察 Linux 返回值、`EINTR` 或 handler 返回后的透明重放。

## 唯一状态与调用链

`ThreadContext` 独占至多一条 `SyscallRestart`，保存原 `a0..a5`、`a7` 和 ecall PC。完整路径为：

1. blocking handler 被 signal 取消后返回 kernel-private restart sentinel；dispatcher 立即把它收敛成 `SyscallOutcome::Restart`，因此 sentinel 不可能写入 `a0`；
2. trap layer 先在 TrapContext 写入普通 `-EINTR`，再把原 syscall 输入交给当前 Thread；记录必须与已经前移 4 bytes 的 ecall PC 对应；
3. signal delivery 读取实际 handler action。含 `SA_RESTART` 时，构造 frame 前恢复原 syscall 输入与 ecall PC；否则沿用 `-EINTR` 和 post-ecall PC。`rt_sigreturn` 只恢复 frame，不存在第二套 restart 入口。

无可交付 signal、默认终止和已忽略 signal 最终都会消费 replay record。fork/clone child 不继承 record，Thread 退出则随 owner 一起释放。

## 精确 restart 边界

- blocking `wait4`：handler 含 `SA_RESTART` 时透明重放；否则返回 `EINTR`，且 child exit record 未被消费。
- 无 timeout 的 `futex(FUTEX_WAIT*)`：handler 含 `SA_RESTART` 时透明重放并重新比较用户值；否则返回 `EINTR`。
- 带 relative timeout 的 futex WAIT：始终返回 `EINTR`。当前没有 restart-block/absolute deadline record；直接重放会重置相对 timeout 并错误延长等待。
- `nanosleep`：始终返回 `EINTR` 并 copyout `rem`，不因 `SA_RESTART` 自动重启。

该边界只覆盖已经证明的调用链，不宣称 Linux 的完整 restart syscall 集合。

## 固定 musl consumer 证据

固定 musl v1.2.6 consumer 先用普通 handler 验证 futex、`nanosleep`、`waitpid` 的 `EINTR/rem/reap`，随后设置 `SA_RESTART` 并验证：

- 无 timeout private futex 在 handler 返回后继续等待，worker wake 后返回 0；
- blocking `waitpid` 在 handler 返回后继续等待并消费 child status 25；
- `nanosleep(500ms)` 仍返回 `EINTR` 和有效 `rem`；
- handler 累计执行六次，所有 pthread worker 正常 join，最终输出唯一成功标记。

consumer 只证明这些实际执行路径，不是通用 Linux/POSIX conformance test。

## 验证

`make verify` 执行 AST 架构围栏、workspace check/clippy、三组件构建、ELF 静态检查、Rust init QEMU `-smp 1/3/8` 冷启动，以及固定 musl consumer 冷启动。静态审计同时确认内部 sentinel 只有 dispatcher 可见、所有 Thread 构造器都初始化空 record、record 只在 signal delivery 消费。

本阶段同时建立后续 tracer bullet 的输入基线：BusyBox 官方 release `1.37.0`、固定 tarball SHA-256 与唯一 `user/base/busybox.config` 已可用未修改的上游源码和固定 musl sysroot 构建为静态 RISC-V `ET_EXEC`。该 gate 只证明 source/config/toolchain/ELF 边界，不将 BusyBox 放入当前 init 镜像，也不宣称 runtime syscall 已满足。
