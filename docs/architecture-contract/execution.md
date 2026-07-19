# 执行域契约

## Owner

- `arch` 独占寄存器布局、context switch、trap decode、MMU encoding、local interrupt 与 architecture fail-stop。
- `entry` 独占 raw boot/trap callback ABI；`cpu` 独占 logical identity/lifecycle/deferred bitset。
- `timer` 独占 per-CPU deadline；`TimerQueue` 独占 Process timer record/deadline index；
  `WaitRegistry` 独占 wait ID、registration 与 sharded source/deadline indexes。
- `ProcessorTopology` 独占 current/runqueue/mailbox；`task::memory_barrier` 独占 request/completion generation；`ProcessGraph` 独占 proc snapshot 与 creation publication。
- 每个 Thread 的 `ContextOwner<UserContext>` 独占稳定 pointer 与 mutable transaction
  capability。AArch64 pointer 绑定到同一 Thread `KernelStack` 顶部保留页，生命周期随 stack
  owner，不建立 user page-table mapping；RISC-V 继续独占 supervisor trap-context VA，
  AddressSpace 只在 create/exec/retire seam bind、rebind 或 unmap。
- RISC-V `sstatus.FS` 是用户 FP register image 的唯一 ownership state；不得增加平行 lazy flag/cache。Off 首用只能由精确 F/D/FP-CSR instruction decode 激活，Dirty entry 必须在 kernel 使用 FP 前保存为 Clean image。
- user trap entry 必须先把用户 `sstatus` 及 Dirty FP image 固化到 `UserContext`，再把 live
  `sstatus.FS` 设为 Dirty 交给 kernel LP64D execution owner。kernel trap 与 context switch 都会
  保存 FP callee state；若让用户 Off 状态进入 kernel，首条 FP save 会再次陷入同一
  kernel trap entry，形成无法记录的递归异常。
- `__kernel_trap` 是 `stvec` direct-mode 的唯一 kernel entry，必须在 assembly 源中
  4-byte aligned。RVC 只保证普通 label 2-byte aligned；若 entry 落在 `address % 4 == 2`，
  `stvec` 会清除低两位并跳到前一条 compressed instruction，使任意首次 interrupt 进入
  不可恢复的递归 exception。
- AArch64 live q0-q31/FPCR/FPSR 由当前 Thread 的 `KernelContext` 唯一拥有；普通
  user/kernel trap 必须零 vector load/store。signal capture/restore、clone inheritance 与
  exec reset 是仅有的额外边界，必须在固定 assembly symbol 中临时开启 FPEN、完成单次
  传输并在返回 Rust 前关闭，禁止 lazy flag、每 trap copy 或 kernel Rust FP/NEON codegen。
- AArch64 lower-EL trap 只在常驻 TTBR1 kernel stack 上暂存 x9-x11，并以 effective
  `SP_EL1` 加编译期固定 offset 直接取得 `UserContext`；普通入口恰好执行一次 integer
  `ADD`，不得读写 metadata pointer 或 `CONTEXTIDR_EL1`。`TPIDR_EL0` 是唯一 thread
  pointer owner，user restore 把未开放的 `TPIDRRO_EL0` 固定为零。

## Interface

- generic callers 只能使用 typed `BootContext`、`UserContext`、`KernelContext`、`TrapEvent`、`CpuId`、`CpuSet` 与 semantic MMU permissions。
- `trap` 只做事件投递和 user-return orchestration；不得读取 CSR、计算 interrupt-controller context 或拥有 syscall state。
- 普通 user return 不得执行 `fence.i` 或 full `sfence.vma`；instruction publication 属于 executable mapping transaction，translation reuse 属于 ASID retirement。
- syscall trap 只通过窄 transaction 读取 `a7/a0..a5/sepc`、写 completion `a0`，并在最终
  user-return 唯一发布 CPU-local metadata；禁止恢复 `load/set_user_context` 全量复制路径。
- local IRQ guard 不可跨 CPU 移动。interrupt-safe lock 内禁止阻塞、调度、可失败分配或等待 remote
  completion；无锁 scheduler idle 是唯一允许持有纯 local IRQ guard 进入 guarded WFI 的路径。
- boot CPU 必须在设备初始化前建立 membarrier per-CPU state；否则启动期 SSIP handler
  会永久等待未发布的 `Once`。随后同步 root block completion 可临时补开
  architecture-local external/software source 并执行 architecture-owned WFI。scheduler idle
  必须让 local IRQ guard 跨过同一 WFI seam：状态复查与 task select 全程关中断，assembly
  临时开中断等待并在返回前再次关闭，最后才由 guard 恢复旧状态。hardirq 发布的
  deferred software interrupt 是已确认 device edge 的耐久 wake token，但领域 deferred consumer
  仍只能在 user-return/idle safe point 执行。可确认一次性 edge 的 guarded WFI 具有唯一 linked
  PC/resume identity；IRQ 若在
  enable 后、WFI 前被确认，kernel trap entry 必须将 RISC-V `sepc` 或 AArch64 `ELR_EL1`
  精确推进到 resume PC，禁止返回后消费已经不存在的 edge 而永久睡眠。不使用 flag、MMIO
  poll 或第二个 IRQ owner。assembly 返回时 local IRQ 必须仍关闭；bootstrap wrapper 或 scheduler
  guard 随后精确恢复原 local IRQ mask/source，并保持 timer source 不变，
  直到既有 scheduler enable seam 完成其 owner 初始化。
- wait/timer/process creation 只允许锁内签发 ID、捕获已预留快照与无失败 commit；
  `Vec::try_reserve`、`FallibleMap::try_prepare`、TCB/address-space 构造必须在 owner guard 外完成。
- ITIMER_REAL replace、POSIX create 与 POSIX replace 只能使用同一个 timer transaction protocol：
  timer owner lock 内 plan 后必须先解锁，prepared storage 在锁外创建，publication 固定取得
  `ProcessGraph → TimerQueue` 并复查 Process/thread lifecycle。OOM、lifecycle 失败与不可复用 retry
  直接 Drop prepared owner；POSIX ID collision 只允许 retarget 并复用同一未发布 node。
  blocking advisory/record lock acquisition 以零 storage 先复查 conflict，只有 `NeedsStorage`
  才释放 registry/VFS owner 锁外扩容并重入验证。
- target-specific façade 必须保持静态分派；禁止 trait object、runtime table 或 parallel compatibility implementation。
- AArch64 普通 user return 只切换 TTBR0/ASID，TTBR1 kernel root 固定；不得增加 KPTI 兼容路径。SVE/SME、PAuth 与 MTE state 不在 context ABI 中，因此不得向 EL0 公布对应能力。

## Failure and cleanup

- context、CPU index、trap class 或 queue ownership 不一致时 fail-stop；不能回退到 boot CPU 或丢弃错误。
- context transaction 并发 claim 或 retire 后访问必须 fail-stop；clone/fork 是唯一允许的完整
  UserContext snapshot，signal/restart/sigreturn 必须在同一 owner 上原地完成且 copy fault 不发布 handler。
- deferred consumer 必须有界并可续批；hardirq 只确认硬件与发布 work，不执行无界领域逻辑。
- `cpu::deferred` 对每个 logical CPU 的 bitmap 只在空→非空 transition 调用一次
  `platform::notify_self`；bitmap 非空时已有 edge 或当前 hardirq continuation 负责抵达 safe point，
  重复发布只能合并 bit。缺少 transition 条件会让 AArch64 self-SGI 在 console raw ring 尚可读时
  无限重入，idle safe point 永远不能消费输入。
- supervisor software interrupt 只负责先确认 local SSIP、再完成同步 memory-barrier request；这是
  共享 deferred wake 与 remote membarrier IPI 的唯一 SSIP acknowledgement owner，deferred bitmap
  consumer 不得清除 SSIP。kernel-trap 不得消费任何领域 deferred work。唯一 consumer safe point
  是所有 event handler 已返回后的 user-return 与 local IRQ 已关闭的 scheduler idle loop；否则 SSIP 可在 syscall 持有
  普通 driver/KERNEL_SPACE lock 时同 CPU 重入并永久自旋。
- console deferred input 从无界 drain 收紧为每批最多 256 bytes，console waiter wake 每批最多 32 个；任一预算耗尽都重新发布合并 `Console` bit，下一批重新复查 owner 状态。
- 未提交 wait/timer node、proc snapshot Arc 与 creation TCB 分别由 prepared transaction 唯一拥有；
  OOM、key/lifecycle 复查失败直接 Drop。wait ticket/PID 失败只烧掉唯一 ID，不留下
  registry/graph membership；RISC-V 共享 mm child 失败必须显式删除临时 trap-context
  mapping，AArch64 由未发布的 KernelStack owner 一并回收 context page。
- armed timer reset 必须复用现存 record/deadline node；staging 与 final owner 间 deadline 到期时只能零 mutation 返回 retry，再锁外补 node，不能把原本零分配的 reset 退化为虚假 ENOMEM。
- timer transaction structure gate 要求 duplicated skeleton/executor/explicit policy/final recheck 为
  `0/1/3/1`；host protocol tests 必须覆盖 prepare-window lifecycle race、prepare OOM 与 collision reuse。
- external hardirq 每次最多处理固定 64 个 claim，且不得为探测 backlog 预读第 65 个；预算外 source 保持由 interrupt controller pending，并在返回后自然重触发。
- 同步 remote translation fence 失败必须在 backing owner 仍被保留时 fail-stop；执行域不得把未确认完成的 fence 当作 cleanup completion。
