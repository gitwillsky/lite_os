# 执行域契约

## Owner

- `arch` 独占 register layout、trap decode、context switch、MMU/ASID、本地 IRQ 与架构等待；generic trap 不读 CSR 或解释 ESR。
- `execution::entry` 只解码 raw ABI；`execution::cpu` 独占 topology/lifecycle；`execution::deferred` 独占内核 merged work bitmap；`execution::barrier` 独占 CPU rendezvous generation。
- native `thread::SchedulingState` 独占 run state、Ready generation、等待身份、切换计数及 restricted fault 计数；`Policy` 独占 active slice、权重快照与 normal/restricted CPU 时间。
- `thread::processor` 独占 per-CPU current/runqueue/mailbox/pending handoff；Ready projection 不另存一份可人工同步的运行状态。
- Linux signal/restart、进程 graph、wait registry 和 process timers 只属于 linuxd；设备完成工作只属于 devmgr。service-sync 不消费内核可变 Rust 状态。

## 上下文与返回

1. native thread 创建时绑定 context 与 typed stack/address-space backing；普通 trap 只做短寄存器 transaction。
2. `ThreadOwner::before_user_return` 在最终 IRQ mask 前完成，允许阻塞或要求退出。
3. `disable_for_transfer` 之后只允许非阻塞 snapshot、ASID activation 和硬件安装；所有 Arc 必须在 noreturn 前显式释放。

RISC-V supervisor trap-context mapping 退休必须完成 translation fence；AArch64 context 随 kernel stack owner 退休。
illegal-instruction 分类与最终提交使用短 context transaction，读取用户指令在两者之间，禁止持有 context claim 获取可阻塞的 VM 锁。

AArch64 lower-EL entry 必须在进入 Rust 前以 TTBR1 kernel root 替换 TTBR0 并执行 ISB；ASID 0 保留，kernel root 的低用户地址范围为空。
返回只安装 owner 授权的 user token，TTBR1/stack 保持永久映射。boot identity 翻译在 kernel root 激活时全量撤销，不能留下 ASID 0 的用户全局映射。
该统一软件隔离不要求 PAN；禁止恢复未经 baseline 验证的 `msr PAN` 或 feature fallback 双轨。
用户地址复制只能经过 VMO/backing 固定与高 direct-map 别名，不借助临时关闭隔离直接解引用。

RISC-V 先固化用户 sstatus/Dirty FP，再启用 kernel FP；kernel trap label 4-byte aligned。
AArch64 live q0-q31/FPCR/FPSR 由 KernelContext 唯一拥有，ordinary trap 零 vector copy；扩展状态不在 ABI 中就不得向用户公布。

## 等待、IRQ 与清理

- Ready publication/retirement token 必须在同一 SchedulingState transaction 中交给唯一 per-CPU committer；旧 generation 不可 dispatch，遗失 token fail-stop。
- Blocking/Preempting 到 Blocked/Ready 的提交只在 outgoing context 保存后执行；pending slot 保活 outgoing owner，next/idle continuation 恰好消费一次。
- IRQ guard 不可跨 logical CPU，持有 spin/IRQ-safe guard 时禁止阻塞、可失败分配或等待 remote completion。
- software interrupt 先确认，再完成同步 barrier/TLB 请求；kernel trap 不消费普通 deferred 领域工作。consumer 只在 user-return/IRQ-closed idle safe point 执行。
- deferred bitmap 空→非空才发送 edge；缺失该条件会反复 self-IPI，饿死真正 consumer。预算耗尽必须重新发布工作。
- guarded WFI 的 enable→WFI 已确认 edge 必须跳到唯一 resume PC；返回时 IRQ 仍关闭，外层 guard 恢复原 mask。
- 服务等待使用 native thread park/unpark 与 `WaitCompletion` arming 握手；完成早于 arming/睡眠仍必须返回，不能靠下一 tick 或 polling 兜底。
- fault/trap、OOM、exit 和 kill 只能由对应 owner 退休 context、wait membership 与 observer；同步 fence 未确认时不得回收 backing。

Linux timer transaction、权限锁序和进程退休的具体契约分别由[进程契约](process-scheduling.md)及[生命周期](process-lifecycle.md)维护。
