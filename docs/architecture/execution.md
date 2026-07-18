# 执行域当前架构

## 当前设计

- `arch` 以静态 façade 暴露 `UserContext`、`KernelContext`、`TrapEvent`、MMU、local interrupt 与 fail-stop mechanism；内部寄存器布局不泄漏。
- `entry` 是 raw boot/trap callback ABI 的唯一 codec。boot 生成 typed `BootContext`，trap 生成 semantic event。
- `cpu::CpuTopology` 唯一拥有 hardware identity 到 logical `CpuId` 的映射及 possible/online/active lifecycle；领域状态按 logical CPU 建槽。
- `trap` 只协调 syscall、fault、timer、software 和 external interrupt 的领域投递。CSR/cause decode 留在 architecture backend。
- timer hardirq 只发布合并的 per-CPU deferred work；deferred context 处理调度 tick、deadline、设备 work 和 signal consequence。
- `LocalIrqGuard` 与 `IrqMutex` 用 RAII 表达 local interrupt nesting；interrupt-safe guard 内禁止阻塞或调度。

## Known limits

- 当前没有 runtime architecture dispatch；这是有意的静态零成本设计。
- 当前 target-specific execution 实现只有 RISC-V64。
