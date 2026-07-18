# 进程与调度契约

## Owner

- Process 独占共享资源与聚合 accounting；Thread 独占 execution/signal/scheduling context。
- SchedulingState 独占 run membership；ProcessorTopology 独占 per-CPU runqueue/current/mailbox projection。
- TaskManager process graph 独占 identity、parent/child、group/session、exit/wait 与 process timer relation。
- IndexedWaitQueue 独占全部 wait registration 与 source index；signal disposition/pending 分别由 Process/Thread 对应 signal state 独占。

## Interface

- 调度路径只使用 logical `CpuId`/`CpuSet`；不得把 hardware identity 保存为 affinity、processor 或 mask。
- Ready transition/retirement token 必须直接交给唯一 commit seam；不得缓存、遗忘或从全局 task 表重建 projection。
- signal selection、permission、generation 与 job-control consequence 必须在 process-graph transaction 内线性化，锁外才执行 wake/notification。
- syscall 只能请求 task façade；不得访问 scheduler container、process graph lock 或 signal internal state。

## Failure and cleanup

- block publication 在 SchedulingState 切换前完成全部 allocation 与 pending-event recheck，避免 lost wakeup。
- exit 顺序保持 robust cleanup、graph removal、clear-child-tid/futex wake；exec/vfork/group-exit 必须明确 point of no return。
- consequence 可能 drop Arc、OFD、waiter 或发送 signal 时，必须在 owner lock 外执行。
