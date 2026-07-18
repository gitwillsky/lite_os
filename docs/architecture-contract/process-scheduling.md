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

- block publication 先用短 registry 锁签发唯一 ID，锁外准备全部 entry/index nodes，再在同一 owner 下复查 pending event 并无失败提交，避免 lost wakeup；取消/OOM 只烧掉 ID。
- clone/fork/vfork 的 TCB、graph node 与 RLIMIT snapshot storage 在 `process_creation` 外准备；
  最终 guard 内只捕获已预留 snapshot、复检 limit 并提交 graph。snapshot backing OOM
  属于 memory failure 并由 syscall 映射为 `ENOMEM`；只有 RLIMIT_NPROC/PID exhaustion
  映射为 `EAGAIN`。Thread clone 的 Linux
  best-effort TID stores 完成前 child 保持 `New`；pre-activation group stop 使用
  `Stopped(New)`，SIGCONT 只能恢复 `New`，最终才在 graph owner 下进入 Ready/Stopped。
  若期间已提交 group-exit，任何 scheduler state 都必须继承 kernel SIGKILL consequence，
  不得逃离既有 group exit。
- exit 顺序保持 robust cleanup、graph removal、clear-child-tid/futex wake；exec/vfork/group-exit 必须明确 point of no return。
- consequence 可能 drop Arc、OFD、waiter 或发送 signal 时，必须在 owner lock 外执行。
- exit staged 的 parent/init child waiter 必须按来源各自 exactly once drain；跨来源 TID 没有排序契约，不得为合并它们扩大通用 ordered-storage interface。
