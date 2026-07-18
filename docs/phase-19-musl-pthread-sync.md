# Phase 19：musl pthread 同步与 timedwait

> 本文保留 Phase 19 历史边界；signal-interrupted wait 后续结论见 [Phase 20](phase-20-signal-interruption.md)。

## 固定 consumer

继续使用 musl `v1.2.6` commit `9fa28ece75d8a2191de7c5bb53bed224c5947417`，不修改 libc。`scripts/fixtures/musl/musl-smoke.c` 在 Phase 18 的 create/join 路径上增加：

1. parent/child 通过同一 `pthread_mutex_t` 与 `pthread_cond_t` 完成双向交接；
2. parent join child，验证 clear-child-tid 与 private futex wake 仍成立；
3. parent 使用 absolute `CLOCK_REALTIME` deadline 调用 `pthread_cond_timedwait`，musl 换算后通过 relative `FUTEX_WAIT_PRIVATE` timeout 进入 kernel，最终必须返回 `ETIMEDOUT`。

成功标记唯一为 `LiteOS musl pthread sync ok`。这是真实 libc consumer 冷启动围栏，不是内核私有 syscall 或 libc patch。

## 唯一等待 owner

原 deadline queue 与 futex queue 合并为 TaskManager 拥有的唯一 indexed wait registry。每个 blocked task 只发布一个 registration ID；registration 可选进入 futex `(TGID,uaddr)` 索引、deadline 索引或两者。

wake 和 timeout 都先在同一 `IrqMutex` 下移除 registration 及所有索引，再用 `SchedulingState` 中的唯一 membership 完成 `Blocking -> WakePending -> Ready`。因此：

- futex compare 与 enqueue 之间无 lost wakeup；
- 显式 wake 与 deadline expiry 只能有一方消费 registration；
- task 恢复后从 `WaitResult` 唯一区分 `Woken` 与 `TimedOut`，不从队列残留状态推断。

## Idle timer 契约

timer hardirq 只在 `HartTopology` 的 per-hart pending word 发布 deferred work 并设置 SSIP。user-return 和 scheduler idle 都调用同一 consumer，负责消费到期 registration 并请求调度。

当所有 task 都 blocked 时，idle 在 SIE=0 下完成 deferred work、mailbox drain、task select 和 WFI；WFI 因 locally-enabled timer/IPI 退出后，再短暂打开 SIE 投递 pending trap。该顺序同时避免：

1. SIE 一直关闭导致 hardirq 永不发布 deferred work；
2. 在 WFI 前开中断，trap 提前返回后 hart 再进入 WFI 的 lost wakeup。

## 精确边界

`futex` 仍是 **Partial**：支持 WAIT/WAKE、PRIVATE flag 和 WAIT relative monotonic timeout，但不支持 requeue、PI、bitset 或 signal interruption。consumer 只证明固定 create/join、mutex/condition 与 condition timedwait 路径，不声称完整 pthread 或通用 musl 兼容。

## 验证

`make verify` 执行架构围栏、workspace check/clippy、三组件构建、ELF 静态检查、Rust init QEMU `-smp 1/3/8` 冷启动，以及固定 musl pthread sync consumer 冷启动。
