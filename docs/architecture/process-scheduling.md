# 进程与调度当前架构

## 当前设计

- Process 拥有共享地址空间 handle、fd table、credentials、limits、cwd 与聚合 runtime；Thread 拥有执行上下文、mask、pending signal、TLS 与调度 membership。
- SchedulingState 是 runnable/blocking/stopped membership 的唯一事实；Ready transition token 在同一 lock lifetime 内更新 per-CPU runqueue projection。
- `ProcessorTopology` 拥有 per-CPU current、runqueue、mailbox 与 load projection。远端 runnable 只经 logical target mailbox 和 platform IPI 交付。
- 普通 yield/block 的 scheduler handoff 直接在 outgoing task 上选择 next Ready owner，并执行一次
  `task -> task` kernel context switch。被保存的 outgoing owner 与 IRQ restore consequence 暂存在
  per-CPU pending slot，由 next task 的 continuation 唯一提交；next 首次运行也先经过同一
  scheduler trampoline。idle 只在没有 runnable successor 且 outgoing 确实 Blocking/Stopped 时进入。
- runqueue 为空但 outgoing 仍为可继续的 Preempting/WakePending 时，scheduler 原地恢复
  Running/current，context switch 数为零。生产成本门禁以 1024 次 runnable handoff 约束
  kernel context switch 从旧双跳的 2048 次降为 1024 次，idle entry 从 1024 降为 0。
- TaskManager process graph 拥有 PID/TID、parent/child、creator Thread、process group/session、
  wait event、timer index 与 process lifecycle transaction；内部维护 direct-child、global TID、
  creator-dependent 与 `(SID,PGID)` exact-membership indexes，使 exit/wait/signal lookup 只触达
  受影响集合。
- `WaitRegistry` 统一拥有 futex、deadline、pipe、poll、signal 和 socket wait registration；
  16 个 source shard 允许无共同 source 的 publication/wake 并行。multi-source wait 仍只有一个
  registration，`Arming/Notified/Armed/Claimed` 状态封闭锁外 readiness 复查与 exactly-once
  completion；发布 membership 前完成全部 fallible allocation。
- signal generation、pending、delivery 与 syscall replay 分层但不复制状态；exit、exec、vfork、robust-list 和 group-exit 均有明确 point of no return 与清理顺序。

## Known limits

- scheduler 当前提供 Linux `SCHED_OTHER`/nice 语义子集，不包含实时调度 class。
- futex PI、PI requeue、WAKE_OP、queued realtime signal 与完整 clone flags 尚未开放。
