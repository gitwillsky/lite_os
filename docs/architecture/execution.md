# 执行域当前架构

## 当前设计

- `arch` 以静态 façade 暴露 `UserContext`、`KernelContext`、`TrapEvent`、MMU、local interrupt 与 fail-stop mechanism；内部寄存器布局不泄漏。
- `entry` 是 raw boot/trap callback ABI 的唯一 codec。boot 生成 typed `BootContext`，trap 生成 semantic event。
- `cpu::CpuTopology` 唯一拥有 hardware identity 到 logical `CpuId` 的映射及 possible/online/active lifecycle；领域状态按 logical CPU 建槽。
- `trap` 只协调 syscall、fault、timer、software 和 external interrupt 的领域投递。CSR/cause decode 留在 architecture backend。
- Thread context owner 在 lifecycle seam 缓存唯一稳定 `UserContext` pointer；AArch64 pointer
  直接落在同一 Thread 的 TTBR1 KernelStack 保留页，RISC-V pointer 继续绑定 supervisor
  trap-context mapping。普通 syscall/trap 使用 register-level transaction，因此不复制完整
  UserContext，也不在热路径取得 AddressSpace lock 或 page-table walk。
- timer/device hardirq 只发布合并的 per-CPU deferred work；kernel SSIP 只确认中断并完成同步
  barrier，领域 consumer 统一在 user-return 或 local-IRQ-closed idle safe point 处理 tick、deadline、
  device work 和 signal consequence。deferred bitmap 只在空→非空 transition 签发一次 local
  software-interrupt edge；bitmap 非空时重复发布只合并 bit，避免 software interrupt 在 safe point
  前自触发并饿死 consumer。
- Process timer mutation 统一经过 timer-domain transaction executor：ITIMER_REAL replace、POSIX create
  与 POSIX replace 只提供显式 policy，plan 锁内读取 storage needs，prepare 锁外分配，最终在
  `ProcessGraph → TimerQueue` 锁序下复查 lifecycle 并发布。
- `LocalIrqGuard` 与 `IrqMutex` 用 RAII 表达 local interrupt nesting；`IrqMutex`/spin lock guard 内禁止
  阻塞或调度。无锁 scheduler idle 是唯一允许持有 `LocalIrqGuard` 进入 guarded WFI 的路径。
- bootstrap completion 与 scheduler idle 共用 architecture-owned guarded WFI 的唯一 linked
  PC/resume identity。idle 在 local IRQ guard 内完成 deferred/mailbox/runqueue 复查与 task select，
  assembly 只在 WFI 期间临时开中断并在返回前再次关闭；hardirq 发布的
  deferred software interrupt 作为已确认 device edge 的耐久 wake token。IRQ 命中
  enable-to-WFI 窗口时，kernel trap entry 将 RISC-V `sepc` 或 AArch64 `ELR_EL1`
  精确推进到 resume PC；不使用 flag、轮询或下一次周期 tick 兜底。
- RISC-V 用户 FP context 仍由 `sstatus.FS` 单一状态机拥有：新任务为 Off，确认 F/D 指令或 FP CSR 首用后转 Initial；trap 只在 Dirty 时保存并发布 Clean，restore 对 Initial 清零、对 Clean/Dirty 恢复。user trap 固化用户 image 后才把 live FS 交给 LP64D kernel owner。
- AArch64 用户 FP/ASIMD 为 eager task state；普通 trap 只关闭 kernel 对 FP 的访问，不复制
  q0-q31。`KernelContext` switch 是调度切换的唯一 vector owner，只有 signal
  capture/restore、clone inheritance 与 exec reset 允许经过固定 architecture assembly
  seam 临时访问 vector file；kernel Rust 不生成 FP/NEON。
- AArch64 lower-EL entry 通过常驻 TTBR1 kernel stack 保全 x9-x11，并以 effective SP
  加固定 offset 的一次 integer `ADD` 直接取得 `UserContext`；不维护 metadata pointer，
  也不再编码地址或访问 `CONTEXTIDR_EL1`。用户 thread pointer 只经 `TPIDR_EL0` 保存/恢复，未开放的
  `TPIDRRO_EL0` 在每次 user return 固定为零。
- AArch64 timer 使用 CNTV，external/timer/software interrupt 统一经过 GICv3 linear claim/EOI token；timer 在 EOI 前重新 arm，software SGI 在 EOI 后完成 memory-barrier rendezvous。
- release 反汇编门禁分别固定 RISC-V integer trap 的 `satp`/fence/FP 成本与 AArch64 ordinary trap 的零 q/FP load-store，并限制 vector 指令只能出现在明确的 context/signal/clone/exec symbols。
- 热路径 benchmark 决策使用 release ELF 的确定性事件计数而不是易受宿主噪声影响的 wall-clock：steady return 只增加一次 ASID CPU-seen atomic load，命中后没有 fence、锁、分配或间接调用；首次跨 CPU activation 的 ASID-scoped fence 不计入 steady trap。

## Known limits

- 当前没有 runtime architecture dispatch；这是有意的静态零成本设计。
- AArch64 只支持 EL0/EL1 执行模型；不开放 EL2 guest、SME、SVE、PAuth 或 MTE state。
