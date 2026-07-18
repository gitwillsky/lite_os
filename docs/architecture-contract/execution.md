# 执行域契约

## Owner

- `arch` 独占寄存器布局、context switch、trap decode、MMU encoding、local interrupt 与 architecture fail-stop。
- `entry` 独占 raw boot/trap callback ABI；`cpu` 独占 logical identity/lifecycle/deferred bitset。
- `timer` 独占 per-CPU deadline；`ProcessorTopology` 独占 current/runqueue/mailbox；`task::memory_barrier` 独占 request/completion generation。

## Interface

- generic callers 只能使用 typed `BootContext`、`UserContext`、`KernelContext`、`TrapEvent`、`CpuId`、`CpuSet` 与 semantic MMU permissions。
- `trap` 只做事件投递和 user-return orchestration；不得读取 CSR、计算 interrupt-controller context 或拥有 syscall state。
- local IRQ guard 不可跨 CPU 移动。interrupt-safe lock 内禁止阻塞、调度、可失败分配或等待 remote completion。
- target-specific façade 必须保持静态分派；禁止 trait object、runtime table 或 parallel compatibility implementation。

## Failure and cleanup

- context、CPU index、trap class 或 queue ownership 不一致时 fail-stop；不能回退到 boot CPU 或丢弃错误。
- deferred consumer 必须有界并可续批；hardirq 只确认硬件与发布 work，不执行无界领域逻辑。
