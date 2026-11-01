# 执行域当前架构

## 内核机制

`arch` 静态 façade 提供 `UserContext`、`KernelContext`、`TrapEvent`、MMU、本地中断与 fail-stop。
`execution::entry` 解码 raw ABI，`execution::trap` 只投递语义事件给 native thread/object owner。
`execution::cpu` 独占 logical identity 与 possible/online/active 生命周期；硬件编号不作为领域索引。

`thread` 拥有 native 线程、上下文、Ready generation、等待 membership 和 CPU 时间。
上下文指针在创建时绑定 typed backing；AArch64 位于 TTBR1 kernel stack 的保留页，RISC-V 使用 supervisor trap-context mapping。
普通 trap 只操作所需寄存器，不复制完整上下文，不在热路径重新查找 backing。

1. 陷入后由架构保存状态，native trap 分派 syscall、异常或中断。
2. owner 在返回前完成可能阻塞的处理；退出路径显式退休线程资源。
3. 唯一 `disable_for_transfer` 封闭最终窗口，取得 address-space token/context，释放 Rust 引用，再进行 ASID activation 与 noreturn transfer。窗口内不得阻塞。

## 中断与等待

- hardirq 只确认硬件并发布有界内核工作；software IRQ 先 ack，再完成 barrier/TLB rendezvous。kernel trap 不重入普通领域锁。
- `execution::deferred` 的 per-CPU bitmap 只在空→非空时发一个软件中断；重复发布合并 bit。内核 consumer 在 user-return/IRQ-closed idle safe point 执行。
- `LocalIrqGuard`/`IrqMutex` 维护本地嵌套；guard 不可跨 CPU，锁内不得阻塞或等待远端 fence。
- native idle 在关中断窗口复查 deferred/mailbox/runqueue 后进入架构 guarded WFI。enable→WFI 间已经确认的中断由精确 PC/resume seam 跳过 WFI，避免丢失一次性唤醒。
- outgoing 线程在汇编保存上下文前只进入 pending handoff；next/idle continuation 在保存后提交 Blocked/Ready 后果。Ready 的发布与退休均须消费线性 projection token。

## 系统服务执行

linuxd 的 Linux 线程由 native 服务线程承载：normal mode 执行兼容层，restricted mode 执行应用。
restricted syscall/异常返回 linuxd，后者独占 Linux 寄存器 ABI、errno、signal、restart、ptrace 与进程政策。
用户地址访问经 native VM 操作解析、固定实际 backing；内核机制不解释 Linux 线程状态。

设备 IRQ 与完成分派在 devmgr 的服务线程中执行；netd 的协议工作线程、linuxd 的 pager/deferred 线程也各自拥有工作队列。
没有当前 Linux 线程的服务线程使用 `ServiceThreadTarget` 停驻于自身 native 线程描述符。
不存在启动期驱动 MMIO polling/WFI 的第二条等待路径。

## 架构状态

- RISC-V 用户 FP 只由 `sstatus.FS` 管理：Off 首用经过精确 F/D/FP-CSR 解码激活，Dirty trap 先保存为 Clean，才把 live FP 交给 LP64D kernel。kernel trap entry 必须 4-byte aligned。
- AArch64 ordinary trap 零 q-register load/store；调度 `KernelContext` switch 独占 vector image。signal/clone/exec 只经固定汇编 seam 临时访问 FPEN，kernel Rust 不产生 FP/NEON。
- AArch64 lower-EL entry 在进入 Rust 前把 TTBR0 切换到 TTBR1 kernel root，选用保留 ASID 0。该 root 的低用户地址半区为空，用户地址无法在特权代码中直接解引用；user-copy 使用固定页的高 direct-map 别名。
- user return 才恢复被授权的 TTBR0/ASID；TTBR1 与 kernel stack 永久有效。此路径支持 ARMv8-A，不依赖 PAN 扩展，也没有按 CPU feature 选择的并行实现。
- ASID 按实际硬件容量换代；steady activation 不锁、不分配、不 fence，换代待处理的 full local fence 由同一 owner 消费。TTBR0 切换不释放仍在使用的 logical ASID reservation。

## 范围与验证

AArch64 只支持 EL0/EL1，不公开 SVE/SME、PAuth、MTE 或 EL2 guest 状态。
release ELF 门禁检查实际 trap/FP/ISA；host 调度状态测试验证线性 token、stale generation 与 CPU 时间不会重复计费。
硬件 TLB、SMP handoff、restricted mode 和 signal 由 guest 门禁裁决，host fixture 不能替代它们。
