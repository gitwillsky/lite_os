# 执行域契约

## Owner

- `arch` 独占 register layout、trap decode、context switch、MMU/ASID、本地 IRQ 与架构等待；generic trap 不读 CSR 或解释 ESR。
- `execution::entry` 只解码 raw ABI；`execution::cpu` 独占 topology/lifecycle；`execution::deferred` 独占内核 merged work bitmap；`execution::barrier` 独占 CPU rendezvous generation。
- native `thread::SchedulingState` 独占 run state、Ready generation、等待身份、切换计数及 restricted fault 计数；`Policy` 独占 active slice、权重快照与 normal/restricted CPU 时间。
- `thread::processor` 独占 per-CPU current/runqueue/mailbox/pending handoff；Ready projection 不另存一份可人工同步的运行状态。
  新的本地唤醒在入队事务中先 checkpoint current，再按该有效 entity 决定 reschedule；实际切换仍等释放锁后的安全点。
  远端 Ready 在原 per-CPU deferred bitmap 发布 RUNQUEUE，空→非空发送 IPI；目标安全点计费 current，
  接管 mailbox 并仅按本批新的有效 entity 裁决。空/过期批次不拿已有 runqueue root 伪造一次新唤醒。
  不得拿未计入 active slice 的旧 vruntime 比较，否则同 CPU 的跨服务回复仍可能等到下一周期 tick。
  与固定 Linux 的本地 `wakeup_preempt_fair` 先更新 current、远端 `sched_ttwu_pending` 在目标 CPU 裁决的顺序一致；
  Preempting 线程以 SwitchOut 原因回队：本地回队不请求反抢刚选定的 successor，否则主动 yield 会在用户指令恢复前被撤销，
  造成等待发布饥饿。WakePending 表示保存上下文前已经发生的真实唤醒，仍以 Wake 裁决抢占。
  迁到其他 CPU 的回队仍通知目标接管。移除旧 current、选择/恢复新 current 时清除旧上下文的 reschedule 请求；
  新 Running owner 的 affinity/stop 请求在同一 state guard 之后发表，不能被清掉。不强制每次 wake 切换，也不观察其他 CPU 的 current。
  未消费的合并 bit 保活工作，take 后的新发布获得新 edge；SSIP/SGI 仍只由 trap handler 确认，barrier 不丢通知。
  无新锁、队列、分配或 Atomic；ReadyCause 只随调用栈传递，RUNQUEUE 复用既有 owner。
  无工作时保持原子空路径；actual deferred host fixture 验证目标隔离、合并与重新签发，CFS 测试保护条件抢占，
  音频跨服务运行时与调度公平性门禁验证延迟及公平性，不新增或放宽阈值。
- 每个 CPU 的 `placement_vruntime` 是独立、单调的虚拟时钟；有效 current/Ready root 更新它，撤销 current
  前必须包含刚结束的 slice，不能让空 runqueue 把时钟停在初始值。idle 不重置该时钟。
  迁移在 SchedulingState guard 内把 `Policy.vruntime - source_floor` 换算到 target_floor，再对真正睡眠者
  应用已有的有界 credit；不得直接比较不同 CPU 的绝对坐标，也不得把换算计入实际 CPU 时间。
  Ready affinity 迁移与 Preempting/WakePending 由状态中的 CPU 确定 source；Blocked 的 source 由上次执行 CPU
  确定，begin_runtime 写入与 Blocking/Blocked 的锁交接保证该值稳定。generation 与新坐标一同发表，旧 entry 仍按 generation 淘汰。
  不新增状态、锁、Atomic 或分配，只读原有 floor；实际 Policy 夹具保护正/负 lag、睡眠 credit 与 CPU 计费不变。
- channel_call 的成功回复优先在 reply CPU 唤醒 caller；server 即将再次等请求，局部交接避免把就绪回复
  留在另一忙 CPU 等下一 tick。该提示复用 TaskEvent/TaskWaitTarget，仍验证精确 wait membership、active 与 affinity，
  不强置抢占或改变权重。peer close 与普通 event 维持正常选核，未保存的 Blocking context 仍沿原 handoff 完成。
  没有额外线程、队列、锁或状态；实际音频的原始四 CPU/八路、1024-frame 软件缓冲门禁验证端到端延迟。
- Linux signal/restart、进程 graph、wait registry 和 process timers 只属于 linuxd；设备完成工作只属于 driver-host。service-sync 不消费内核可变 Rust 状态。

## 上下文与返回

1. native thread 创建时绑定 context 与 typed stack/address-space backing；普通 trap 只做短寄存器 transaction。
2. `ThreadOwner::before_user_return` 在最终 IRQ mask 前完成，允许阻塞或要求退出。
   首次进入、阻塞恢复和抢占恢复共用 `trap_return` 循环：owner 处理后消费 deferred 与重调度请求；
   切出再恢复必须重新检查 owner。仅在 trap handler 尾部检查会漏过 handoff 新发表的本地唤醒，
   即使该线程按 CFS 应优先运行，也会被拖到下一次 IRQ。
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
小块尺寸桶覆盖至 2 KiB，保留到服务进程 VMAR 退休，成本是小块并发高水位。
整页请求与完整 native channel 回复各使用独立映射，每种尺寸最多保留一个备用块（4 KiB + 64 KiB）；
并发 RPC 结束后的多余块立即撤销，不能用长期 slab 保留一页工作缓冲的突发峰值而耗尽小内存实例。
整页桶覆盖带事务头、代际与长度前缀的半页 PCM 数据，顺序提交复用它以减少 VMAR/fence 成本。
中等尺寸与超出 chunk 的分配仍为独立映射，在 dealloc 时撤销；撤销失败 fail-stop，不能丢失最后的映射回收责任。
chunk 基址按 max(page, block) 对齐；较大对齐的 prefix/suffix 在发布地址前裁剪，活跃块不得重叠。
裁剪拒绝先撤销整个未发表区间再返回空指针；回滚失败 fail-stop，不能发布失去所有权的部分映射。
缺少完整消息备用块时，顺序 RPC 的每次回复都要创建/撤销 VMAR 并跨核 fence，放大音频等有期限的数据流延迟。
实际分配器 fixture 验证大块对齐、复用时无新增映射、活跃块隔离、突发后的有界保留及独立映射退休；
实际 RPC 成本与容量分别由音频/桌面、musl 地址空间压力门禁验证。

## 原生任务观测

- `object::Process` 的 lifecycle 锁同时保护存活线程索引与退出累计计费；最终 runtime 与线程摘除同一事务提交，
  不丢失已退出线程，也不与并发查询重复计费。启动失败只回滚登记，不改变未启动执行体的状态。
- `TASK_RUNTIME` 查询包括已退出线程；`TASK_STATS` 返回映射字节、映射驻留字节和存活线程数，均要求 INSPECT。
  统计由实际 task/VM owner 提供，不在 root-server 或桌面另建人工同步计数。
- Job 默认持有 `ENUMERATE`：`JOB_CHILDREN`/`JOB_PROCESSES`（另需 INSPECT）返回存活直属成员的升序 KOID，
  `OBJECT_GET_CHILD` 按 KOID 取成员句柄，rights 不得超出 Job 句柄。只读观察者因此只能取得只读成员，不能借枚举提权。
  KOID 在锁内复制、锁外写回用户，结果是时点快照；成员随后结束时取句柄返回 `ERR_NOT_FOUND`。
- root-server 交给 linuxd 根 Job 的 `INSPECT | ENUMERATE | TRANSFER` 句柄（对应 Fuchsia `RootJobForInspect`），
  没有 MANAGE、READ、WRITE。linuxd 每次请求遍历任务树，新增服务自动可见；linuxd 所在 Job 中除 linuxd 自身外的成员是
  Linux 进程或映像共享进程，前者由 /proc 呈现，二者都不进入快照。不向应用传递原生句柄。
- `task-observer-protocol` 拥有版本化查询/回复格式与端点约定。linuxd 的 `system::task_observer` 经 `system::endpoint`
  在 devtmpfs 挂载后、init 发布前发布 `/dev/task-observer` socket 节点（root:adm 0660）；devtmpfs 对 Linux 只读，
  节点从发布起就是最终权限，Linux 进程无法抢先占用路径。连接与发送由 VFS socket inode 写权限按 fsuid 与附加组授权，
  组外进程得到 `EACCES`。端点放在 linuxd 自有的 devtmpfs 上（同 Linux `/dev/log`），因为 `/run` 由 init 脚本稍后挂载。只返回服务名称/运行状态/汇总资源量，
  不返回地址、寄存器、凭据、映射明细或管理 capability。请求不持久化客户端状态；socket 队列沿用现有配额，
  回复不能立即发送时丢弃，不阻塞其他客户端。快照受单个 datagram 上限约束（80 字节/记录），原生服务数量远低于该界。
- 原生 KOID 与 Linux PID 分属不同身份空间。原生服务按自己的承载进程计费，Linux 应用及其 syscall 消耗归实际成员承载线程；
  不再把相同应用的承载线程计入服务。Linuxd 工作线程是其服务的展开明细，系统总线程数不重复加它们。
  普通 Linux 进程仍使用标准 procfs/pidfd；获授权的系统桌面工具消费只读服务协议，不新增 Linux syscall、ioctl 或伪造 PID。
- 观测是低频、按请求执行的冷路径。锁序/稀疏驻留统计由生产 VMO fixture 与 runtime 验证；不在调度、fault 或 IPC 热路径新增采样。
