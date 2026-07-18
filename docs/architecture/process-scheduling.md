# 进程与调度当前架构

## 当前设计

- Process 拥有共享地址空间 handle、fd table、credentials、limits、cwd 与聚合 runtime；Thread 拥有执行上下文、mask、pending signal、TLS 与调度 membership。
- SchedulingState 是 runnable/blocking/stopped membership 的唯一事实；Ready transition token 在同一 lock lifetime 内更新 per-CPU runqueue projection。
- `ProcessorTopology` 拥有 per-CPU current、runqueue、mailbox 与 load projection。远端 runnable 只经 logical target mailbox 和 platform IPI 交付。
- TaskManager process graph 拥有 PID/TID、parent/child、process group/session、wait event、timer index 与 process lifecycle transaction。
- IndexedWaitQueue 统一拥有 futex、deadline、pipe、poll、signal 和 socket wait registration；发布 membership 前完成全部 fallible allocation。
- signal generation、pending、delivery 与 syscall replay 分层但不复制状态；exit、exec、vfork、robust-list 和 group-exit 均有明确 point of no return 与清理顺序。

## Known limits

- scheduler 当前提供 Linux `SCHED_OTHER`/nice 语义子集，不包含实时调度 class。
- futex PI、PI requeue、WAKE_OP、queued realtime signal 与完整 clone flags 尚未开放。
