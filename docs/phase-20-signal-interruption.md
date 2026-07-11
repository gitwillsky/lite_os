# Phase 20：blocked wait signal interruption

## 目标

在不新增 wait queue 或 signal 专用调度旁路的前提下，使未屏蔽、非忽略 thread-directed signal 可以中断 deadline、futex 和 child wait。userspace 只观察到 Linux errno 与 signal frame，不看到内核 wait ID 或私有 restart code。

## 统一取消协议

signal 发布顺序固定为：

1. 在 Thread 的 pending bitset 合并 standard signal；已忽略 signal 直接丢弃，已屏蔽 signal 只保留 pending；
2. 对当前可交付 signal，按 wait 的唯一 owner 取消 membership：indexed registry 原子删除 registration 及 futex/deadline index，process graph 原子取走 child waiter；
3. scheduler 只通过原 `Blocking -> WakePending -> Ready` 协议发布 `WaitResult::Interrupted`。

每个 blocking path 在 owner lock 内复查 `has_deliverable_signal()`。sender 取消时也先取 owner lock，再持有 signal mask/pending 锁复查并注销 membership，避免 signal 已被另一 hart 交付后误伤新 wait。该顺序同时封闭 signal-before-enqueue 窗口：若 sender 先发布，blocker 拒绝入队；若 blocker 先持有 owner lock，sender 在其入队后消费 registration。

wake、timeout、child exit 和 signal interruption 竞争时，只有先从 owner 移除 membership 的一方能发布 wake result；其他路径发现 stale membership 后为空操作。

## syscall 结果

- `futex(FUTEX_WAIT*)` 在 signal 取消时返回 `-EINTR`；
- `nanosleep` 返回 `-EINTR`，并按 monotonic elapsed time copyout `rem`；
- blocking `wait4` 返回 `-EINTR`，不消费 child exit record；
- trap return 随后在用户栈构造原 Linux RV64 signal frame，handler 返回后恢复 syscall 结果与 `rem`。

## 固定 musl consumer

Phase 19 consumer 继续验证 create/join、mutex/condition 和 timedwait。signal worker 先正常 `nanosleep(20ms)`，再通过 musl `syscall(SYS_tgkill, ...)` 向主线程投递 `SIGUSR1`。验收同时要求：

- raw private futex wait 返回 -1 且 `errno == EINTR`；
- 第二个 worker 中断 `nanosleep(500ms)`，其返回 -1 且 `errno == EINTR`；
- `rem` 非零且小于原 500ms；
- 两个 signal worker 均可正常 join；
- fork child 中断 parent `waitpid`，parent 获得 `EINTR` 后二次 wait 仍能消费 exit status 23；
- handler 累计恰好执行三次；
- 最终唯一标记为 `LiteOS musl pthread signal ok`。

## 精确边界

本阶段不实现 `SA_RESTART`。需要 restart 的 syscall 仍会向 userspace 返回 `EINTR`；后续必须在 trap/syscall 边界保存可重放参数，使用内部 restart 结果，并由实际交付 signal 的 disposition 决定是否重启；不得把内部 errno 暴露给 userspace。仍无 altstack、queued realtime value、process-directed kill 和自动 SIGCHLD。

consumer 还暴露并修复了两个相邻生命周期/ABI 问题：Thread 必须先从 process graph 注销，再用 clear-child-tid 唤醒 joiner；`clone(SIGCHLD, 0)` 必须忽略 flags 未启用的 parent_tid/tls/child_tid 寄存器。否则 musl `pthread_join -> fork` 分别会遇到短暂假多线程状态或错误 `EINVAL`。

## 验证

`make verify` 执行架构围栏、workspace check/clippy、三组件构建、ELF 静态检查、Rust init QEMU `-smp 1/3/8` 冷启动，以及固定 musl pthread signal consumer 冷启动。
