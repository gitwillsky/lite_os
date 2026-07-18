# 执行域契约

## Owner

- `arch` 独占寄存器布局、context switch、trap decode、MMU encoding、local interrupt 与 architecture fail-stop。
- `entry` 独占 raw boot/trap callback ABI；`cpu` 独占 logical identity/lifecycle/deferred bitset。
- `timer` 独占 per-CPU deadline；`TimerQueue` 独占 Process timer record/deadline index；`IndexedWaitQueue` 独占 wait ID、entry 与 source/deadline indexes。
- `ProcessorTopology` 独占 current/runqueue/mailbox；`task::memory_barrier` 独占 request/completion generation；`ProcessGraph` 独占 proc snapshot 与 creation publication。

## Interface

- generic callers 只能使用 typed `BootContext`、`UserContext`、`KernelContext`、`TrapEvent`、`CpuId`、`CpuSet` 与 semantic MMU permissions。
- `trap` 只做事件投递和 user-return orchestration；不得读取 CSR、计算 interrupt-controller context 或拥有 syscall state。
- local IRQ guard 不可跨 CPU 移动。interrupt-safe lock 内禁止阻塞、调度、可失败分配或等待 remote completion。
- wait/timer/process creation 只允许锁内签发 ID、捕获已预留快照与无失败 commit；
  `Vec::try_reserve`、`FallibleMap::try_prepare`、TCB/address-space 构造必须在 owner guard 外完成。
  blocking advisory/record lock acquisition 以零 storage 先复查 conflict，只有 `NeedsStorage`
  才释放 registry/VFS owner 锁外扩容并重入验证。
- target-specific façade 必须保持静态分派；禁止 trait object、runtime table 或 parallel compatibility implementation。

## Failure and cleanup

- context、CPU index、trap class 或 queue ownership 不一致时 fail-stop；不能回退到 boot CPU 或丢弃错误。
- deferred consumer 必须有界并可续批；hardirq 只确认硬件与发布 work，不执行无界领域逻辑。
- console deferred input 从无界 drain 收紧为每批最多 256 bytes，console waiter wake 每批最多 32 个；任一预算耗尽都重新发布合并 `Console` bit，下一批重新复查 owner 状态。
- 未提交 wait/timer node、proc snapshot Arc 与 creation TCB 分别由 prepared transaction 唯一拥有；OOM、key/lifecycle 复查失败直接 Drop。wait ticket/PID 失败只烧掉唯一 ID，不留下 registry/graph membership；共享 mm child 失败必须显式删除临时 trap context。
- armed timer reset 必须复用现存 record/deadline node；staging 与 final owner 间 deadline 到期时只能零 mutation 返回 retry，再锁外补 node，不能把原本零分配的 reset 退化为虚假 ENOMEM。
- external hardirq 每次最多处理固定 64 个 claim，且不得为探测 backlog 预读第 65 个；预算外 source 保持由 interrupt controller pending，并在返回后自然重触发。
- 同步 remote translation fence 失败必须在 backing owner 仍被保留时 fail-stop；执行域不得把未确认完成的 fence 当作 cleanup completion。
