# 执行域当前架构

## 当前设计

- `arch` 以静态 façade 暴露 `UserContext`、`KernelContext`、`TrapEvent`、MMU、local interrupt 与 fail-stop mechanism；内部寄存器布局不泄漏。
- `entry` 是 raw boot/trap callback ABI 的唯一 codec。boot 生成 typed `BootContext`，trap 生成 semantic event。
- `cpu::CpuTopology` 唯一拥有 hardware identity 到 logical `CpuId` 的映射及 possible/online/active lifecycle；领域状态按 logical CPU 建槽。
- `trap` 只协调 syscall、fault、timer、software 和 external interrupt 的领域投递。CSR/cause decode 留在 architecture backend。
- Thread context owner 在 lifecycle seam 缓存唯一 trap-context mapping；普通 syscall/trap 使用
  register-level transaction，因此不再复制 576-byte UserContext、取得 AddressSpace lock 或 page-table walk。
- timer/device hardirq 只发布合并的 per-CPU deferred work；kernel SSIP 只确认中断并完成同步
  barrier，领域 consumer 统一在 user-return 或 local-IRQ-closed idle safe point 处理 tick、deadline、
  device work 和 signal consequence。
- Process timer mutation 统一经过 timer-domain transaction executor：ITIMER_REAL replace、POSIX create
  与 POSIX replace 只提供显式 policy，plan 锁内读取 storage needs，prepare 锁外分配，最终在
  `ProcessGraph → TimerQueue` 锁序下复查 lifecycle 并发布。
- `LocalIrqGuard` 与 `IrqMutex` 用 RAII 表达 local interrupt nesting；interrupt-safe guard 内禁止阻塞或调度。
- bootstrap external wait 的 WFI 拥有唯一 linked PC/resume identity；hardirq 发布的
  pending SSIP 作为已确认 device edge 的耐久 wake token。external/software IRQ 命中
  enable-to-WFI 窗口时，kernel trap entry 推进 `sepc`；不使用 flag 或轮询。
- 用户 FP context 由 `sstatus.FS` 单一状态机拥有：新任务为 Off，确认 F/D 指令或 FP CSR 首用后转 Initial；trap 只在 Dirty 时保存并发布 Clean，restore 对 Initial 清零、对 Clean/Dirty 恢复。纯整数 trap 的 release 指令路径没有 FP load/store。
- user trap 在固化用户 FS/image 后把 live FS 切到 Dirty kernel owner，保证 LP64D Rust、
  kernel-trap entry 与 context switch 不会继承用户 Off 状态并递归陷入。
- release 反汇编门禁固定统计一趟 integer-only user trap：两次 `satp`，零次 full `sfence.vma`、FP load/store 和 return-time `fence.i`。
- 热路径 benchmark 决策使用 release ELF 的确定性事件计数而不是易受宿主噪声影响的 wall-clock：steady return 只增加一次 ASID CPU-seen atomic load，命中后没有 fence、锁、分配或间接调用；首次跨 CPU activation 的 ASID-scoped fence 不计入 steady trap。

## Known limits

- 当前没有 runtime architecture dispatch；这是有意的静态零成本设计。
- 当前 target-specific execution 实现只有 RISC-V64。
