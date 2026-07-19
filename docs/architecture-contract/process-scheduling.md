# 进程与调度契约

## Owner

- Process 独占共享资源与聚合 accounting；Thread 独占 execution/signal/scheduling context。
- SchedulingState 独占 run membership；ProcessorTopology 独占 per-CPU runqueue/current/mailbox projection。
- ProcessorTopology 的 per-CPU pending handoff slot 独占已经保存 context、尚未提交
  Ready/Blocked/Stopped consequence 的 outgoing owner。slot 同时携带原 logical CPU 的 IRQ
  restore token；next task 或 idle continuation 必须恰好消费一次，禁止从 task identity 重建。
- TaskManager process graph 独占 identity、parent/child、creator-Thread、group/session、exit/wait
  与 process timer relation；`parent.children`、global `TID -> TGID`、creator children 与
  `(SID, PGID) -> members` 是同一 graph owner 的 projection，不得复制成第二套 lifecycle state。
- `WaitRegistry` 独占全部 wait registration 与 source index；固定 16 个 shard 只按稳定
  source identity 路由，registration 的 exact key list 是跨 shard claim/cancel 的唯一反向
  metadata。signal disposition/pending 分别由 Process/Thread 对应 signal state 独占。
- `sync::TaskMutex` 独占 task-only blocking owner 的 FIFO ticket、wait chain 与 handoff；task
  domain 只实现 opaque `TaskMutexWaitTarget`，把完整 `(owner address, ticket)` 投影为唯一
  `WaitMembership::TaskMutex`。scheduler adapter 在 processor topology 后只安装一次；缺失、
  重复安装、ticket 回绕或 exact wake mismatch 都必须 fail-stop，不能退回 runnable polling。

## Interface

- 调度路径只使用 logical `CpuId`/`CpuSet`；不得把 hardware identity 保存为 affinity、processor 或 mask。
- Ready transition/retirement token 必须直接交给唯一 commit seam；不得缓存、遗忘或从全局 task 表重建 projection。
- 本地 Ready publication 只有在其 vruntime 严格早于 current 时才发布 reschedule；post-switch
  handoff 重新入队的高 vruntime outgoing 不得反向抢占刚选中的 successor。回归固定验证
  `1_000_000 > 10_000` 不抢占、`1_000 < 20_000` 抢占，相等/stale root 不产生 ping-pong。
  remote Ready sender 只发布 IPI wake edge，不读取 target policy 或强置 reschedule：idle target
  立即 drain inbound，busy target 在下一 tick 按 authoritative Ready projection 进入 CFS 选择。
- runnable successor 已存在时，scheduler 必须在一次 `task -> task` context switch 中转移
  `Processor.current`；只有没有 successor 且 outgoing 不能继续 Running 时才进入 idle。
  outgoing 是可继续的 Preempting/WakePending 且 runqueue 为空时直接恢复 current，不做自我切换。
- idle decision 必须在 local IRQ guard 内依次完成 deferred safe point、mailbox drain 与 runnable
  selection；无 successor 时 guard 继续覆盖 architecture guarded WFI。assembly 临时开中断，trap
  通过精确 WFI/resume PC 修复 enable-to-WFI 窗口，返回前再次关中断，guard 随后恢复旧状态。
  禁止先释放 guard 再执行裸 WFI，也禁止把周期 timer 当作一次性 IPI/deferred edge 的可靠兜底。
- boot CPU 保留 always-armed housekeeping/liveness tick；global deadline/timer queue 的状态 owner
  不变。非 boot CPU 在同一个 idle IRQ guard 内关闭 local timer source，只有选中 runnable task
  后才先写入未来 deadline、恢复 source，
  再切换到 task；remote Ready 的 IPI 必须独立于 timer source 唤醒 WFI。禁止让全部 idle CPU 保持
  周期 tick，也禁止关闭 boot tick 后用轮询或 runtime housekeeping source 迁移补偿。
- signal selection、permission、generation 与 job-control consequence 必须在 process-graph transaction 内线性化，锁外才执行 wake/notification。
- clone/fork/vfork child 必须在发布前从 calling task 的 live architecture context 取得完整 machine snapshot；AArch64 包含 q0-q31、FPCR 与 FPSR。新 task 初始 vector image 和 exec commit 后的 live vector file 必须为零，禁止跨进程映像泄漏；普通 trap 不得承担该同步。
- exit/reparent/wait/TID selection 只能遍历 direct children、creator dependents 或 exact group
  members；禁止退回全 ProcessGraph 扫描。waiter storage 在锁外准备，event claim、signal
  复查与 waiter publication 必须在一次 graph transaction 内完成。正 PID 的 `wait4`
  selector 是 caller input，不是 parent-child index node；并发 waiter 已消费该 child 后，loser
  必须返回 `ECHILD`，不得把 exact PID 缺失误判为 graph corruption 并 panic。
- syscall 只能请求 task façade；不得访问 scheduler container、process graph lock 或 signal internal state。
- TaskMutex wait 不进入 signal-indexed registry，也不接受 signal cancel；owner unlock 或
  publication-window self-wake 是消费该 membership 的唯一路径。wait node/Arc 在 owner spin
  gate 外预分配，final enqueue 与 `Held -> Handoff(ticket)` 在短锁内线性化，wake consequence
  必须在锁外执行。

## Failure and cleanup

- wait ID 由单一 AtomicU64 无锁签发；全部 source node 与 registration storage 在 shard
  lock 外准备。publication 先进入 `Arming`，readiness/backend/signal 复查只在 shard lock
  外执行；并发 source notification 把它变为 `Notified`，arm transaction 按 shard 编号升序
  锁定全部 exact nodes 后才发布 SchedulingState。winner 必须同步删除所有 source/deadline/
  task nodes，禁止 lazy stale membership 或恢复全局 queue。
- clone/fork/vfork 的 TCB、graph node 与 RLIMIT snapshot storage 在 `process_creation` 外准备；
  最终 guard 内只捕获已预留 snapshot、复检 limit 并提交 graph。snapshot backing OOM
  属于 memory failure 并由 syscall 映射为 `ENOMEM`；只有 RLIMIT_NPROC/PID exhaustion
  映射为 `EAGAIN`。Thread clone 的 Linux
  best-effort TID stores 完成前 child 保持 `New`；pre-activation group stop 使用
  `Stopped(New)`，SIGCONT 只能恢复 `New`，最终才在 graph owner 下进入 Ready/Stopped。
  若期间已提交 group-exit，任何 scheduler state 都必须继承 kernel SIGKILL consequence，
  不得逃离既有 group exit。
- clone/fork/vfork 必须在 publication 前预留 process、Thread、parent-child、creator-child、
  TID 与 group membership 的全部节点；owner guard 内一次无失败 commit。任一 OOM 只 Drop
  未发布 token/TCB，不得留下半条 parent、creator、TID 或 group edge。
- exit 顺序保持 robust cleanup、graph removal、clear-child-tid/futex wake；exec/vfork/group-exit 必须明确 point of no return。
- Thread exit 先从 global TID index 移除并把 creator dependents 无分配迁移给同组 reaper
  Thread；最后一个 Thread 退出再把 direct children 无分配迁移给 init。pdeath count 为零时
  不得访问 child Thread collection；session foreground 与 orphan-group consequence 只冻结
  exact group members，并始终按 SIGHUP 后 SIGCONT 在 graph lock 外投递。
- consequence 可能 drop Arc、OFD、waiter 或发送 signal 时，必须在 owner lock 外执行。
- outgoing consequence 不得在 context save 前发布；否则 remote CPU 可恢复仍在执行的 kernel
  stack。context switch 前只发布 per-CPU pending token，restore 后才完成 wait/signal/stop 的
  exactly-once transition。IRQ restore token 若观察到不同 logical CPU 必须在恢复中断前 fail-stop。
- exit staged 的 parent/init child waiter 必须按来源各自 exactly once drain；跨来源 TID 没有排序契约，不得为合并它们扩大通用 ordered-storage interface。
