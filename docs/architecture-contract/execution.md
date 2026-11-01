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
- native-runtime 的启动闭包和结果引用属于 Descriptor，执行时不把其 Box 搬到线程栈；
  THREAD_EXIT 不返回且不展开 Rust 栈，栈持有会在每次 carrier exit 泄漏。正常返回立即释放捕获；
  直接退出由 reaper 确认 TERMINATED 后销毁栈、归还线程引用，最终 Descriptor Drop 释放捕获。
  reaper 不重新取得可能随退出栈遗留的 start guard。

Linux timer transaction、权限锁序和进程退休的具体契约分别由[进程契约](process-scheduling.md)及[生命周期](process-lifecycle.md)维护。

## 系统服务同步

以下 `sync` 指 `service-sync`，其短锁委托 `native-runtime::sync` 的 native futex 互斥锁；
服务线程可被抢占，不能把它换成自旋等待。Linux membership 由 linuxd 适配，未承载 Linux 任务的
服务线程由 ServiceThreadTarget 在自己的 native 描述符停驻。

- `sync::TaskMutex` 独占 task-only blocking owner 的 FIFO ticket、wait chain 与 handoff；
  `sync::TaskEvent` 独占单等待者自动复位事件的 pending/waiter/completion。两者共用
  `sync::task_wait` 的唯一 scheduler adapter：task domain 只实现 opaque `TaskWaitTarget`，把完整
  `(owner address, ticket)` 投影为唯一 `WaitMembership::TaskWait`。`TaskEvent` 先登记 waiter 再复查
  pending，signal 先发布 pending 再取走 waiter，任一交错都不丢失唤醒。event 的 signal/cancel
  必须在同一 waiter lock 内 complete，锁外才 exact wake，不能让迟到 completion 污染新等待。
  TaskWaitMode 明确区分普通、可中断与仅致命信号中断；event 与 mutex 复用相同
  signal predicate 和 exact membership，DMA 与普通 mutex lock 永不中断。
  scheduler membership 发布后再复查 signal，signal/event 都只消费同一 membership 一次；
  中断后已提取的旧 wake 可合法失败，但不能命中新等待。event ticket 从唯一全局
  EVENT_TICKETS 领取，跨同地址对象重建也不复用，不分配、不承载内容 ordering；否则
  仅保活 TCB 的迟到信号会形成生命周期 ABA。此 Atomic 只在阻塞登记时使用。
  Linux scheduler adapter 在任务初始化时只安装一次；缺失、重复安装、ticket 耗尽或
  非可中断 key 的 exact wake mismatch 必须 fail-stop，不能退回 runnable polling。

## Restricted syscall PC

两个 backend 的 native syscall entry 都在 restricted 退出前完成指令 PC 的推进。
linuxd 只解码返回寄存器，并以 saved PC - 4 取得原 syscall 指令地址；不得在 RISC-V 再推进一次。
restart 与 seccomp 消费同一个原指令地址。实际 RISC-V musl/BusyBox/std 运行时验证这个交接，
单独编译不能证明返回指令没有被跳过。

## PLIC level masking

PLIC 的 level source 用全局 priority 0/1 屏蔽与恢复；context enable bitmap 只承载 CPU affinity。
已 claim source 在 complete 时仍保持 context enable，避免控制器忽略完成；unmask 的 priority 写入
同时使 QEMU 重新裁决已有 pending 的 SEIP，不能只改 enable bitmap 后等待下一次设备边沿。
该顺序参照固定 Linux irq-sifive-plic 的 irq_mask/irq_unmask/eoi；不新增中断状态副本。

## 返回硬件视图

ThreadOwner 的返回前处理移交 UserReturn（Enter 一个 ExecutionView，或 Exit）；只投影对象当前 mode，
不新增线程模式副本或运行时架构分派。最终 IRQ mask 之后取得相应根 token；normal/private 的地址边界与
双根撤销寿命由[内存契约](memory.md#受限执行视图)维护。native restricted state 仅可由共享进程成员进入。

## 服务堆与 IPC 缓冲

native-runtime::heap 独占分配器注册与 VMAR，allocator::Classes 在原互斥锁内拥有各尺寸空闲链。
原有小块尺寸桶保留到服务进程 VMAR 退休，成本是小块并发高水位。另一个桶只接收完整 native channel
缓冲，最多保留一个空闲 chunk；并发 RPC 回复结束后，多余 chunk 立即撤销，不能永久保留突发峰值。
中等尺寸与超出 chunk 的分配仍为独立映射，在 dealloc 时撤销；撤销失败 fail-stop，不能丢失最后的映射回收责任。
chunk 基址按 max(page, block) 对齐；较大对齐的 prefix/suffix 在发布地址前裁剪，活跃块不得重叠。
裁剪拒绝先撤销整个未发表区间再返回空指针；回滚失败 fail-stop，不能发布失去所有权的部分映射。
缺少完整消息备用块时，顺序 RPC 的每次回复都要创建/撤销 VMAR 并跨核 fence，放大音频等有期限的数据流延迟。
实际分配器 fixture 验证大块对齐、复用时无新增映射、活跃块隔离、突发后的有界保留及独立映射退休；
实际 RPC 成本与容量分别由音频/桌面、musl 地址空间压力门禁验证。
