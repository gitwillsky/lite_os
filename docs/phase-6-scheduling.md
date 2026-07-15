# LiteOS Phase 6：调度、阻塞与唤醒

> 审计日期：2026-07-11（Asia/Shanghai）
> 代码基线：提交 `3239ce6`（Phase 0–5）
> 验证约束：不维护、不修正、不执行测试；只做构建、静态状态转换审计和非测试 QEMU 启动观察。

## 1. 当前模型

- 每个 active hart 有独占 `Processor`、一个 boxed `dyn Scheduler`、current、idle context、remote inbound mailbox 与近似 queue counter。
- 实际只构造 `CFScheduler`，但 trait 同时要求无调用的 count/query/all-tasks façade，源码还保留从未构造的 FIFO/Priority 实现。
- `TaskStatus` 与真实 current/runqueue membership 分离；`set_task_status` 先写状态，再通过全局 TGID index 猜测是否 enqueue。
- deadline sleep 只在 task 存一个原子 deadline；每次 timer softirq 扫描整个 TGID table，CAS 清 deadline 后再调用状态更新。
- suspend、block、signal stop/continue、timer wake 与 scheduler select 分别直接写状态或队列，没有一处拥有完整转换。

## 2. 已确认问题

| 严重度 | 问题 | 后果 |
|---|---|---|
| Blocker | state、current 与 runqueue membership 不是同一协议 | 同一 task 可重复入队、Ready 但不在队列、Running 仍被远端 enqueue |
| Critical | block 先写 Sleeping/deadline，随后才移除 current | deadline 到期可在真正切出前远端 wake/run，形成双核并发执行 |
| Critical | timer 通过遍历全 TGID table 模拟 wait queue | O(process count) interrupt work、无明确 waiter ownership、扩展后丢唤醒 |
| Critical | terminal/stopped task 的 stale queue entry 没有统一消费规则 | task count 与实际可运行项分离，退出对象可能被重新观察 |
| Major | FIFO/Priority 与 scheduler trait 从未接入 | 多套装饰性模型增加审计面且持续产生 warning |
| Major | `queued_tasks` 只近似本地 heap，不包含 mailbox/current | “load” 名称暗示了并不存在的权威 task count |
| Major | 无 reschedule flag/preemption 协议 | timer tick 只处理 softirq，不触发公平抢占；当前是 cooperative CFS-like yield |

## 3. 目标状态机

`SchedulingEntity` 的单一 `RunState` 同时编码逻辑状态与 membership：

- `Ready { cpu }`：恰好位于该 CPU local runqueue 或尚未 drain 的 inbound mailbox；
- `Running { cpu }`：恰好是该 CPU current；
- `Blocking { cpu }`：已从 current 取下、正在自身栈切出，waker 只能登记 wake-pending；
- `Blocked`：位于一个明确 wait queue；
- `WakePending { cpu }`：blocking 尚未完成但 wake 已发生，由 idle switch-return 完成 enqueue；
- `Stopped`：不在 current/runqueue/wait queue；
- `Exited`：terminal，不得再次 enqueue。

允许的核心转换：

1. create → Ready；enqueue 与 CPU 选择只发生一次。
2. Ready → Running；只有 owner CPU dequeue 成功后可设置 current。
3. Running → Ready；当前 CPU 本地 requeue，禁止在 task 真正切出前远端运行。
4. Running → Blocking → Blocked；wait queue 先拥有 Arc，idle return 完成切出。
5. Blocking → WakePending → Ready；解决 wake-before-switch。
6. Blocked → Ready；waker 从唯一 wait queue 移除后 enqueue。
7. Running/Ready/Blocked/Stopped → Exited；必须先从对应 owner 移除，完整对象不作为 zombie。

## 4. 阶段边界

- 当前只有一个用户 Thread，不宣称实现 work stealing、migration、affinity 或实时策略；remote wake 使用 mailbox + IPI。
- Phase 6 保留一个最小 vruntime runqueue，但在没有 timer preemption 前明确称为 cooperative，不冒充完整 Linux CFS。
- Phase 7 在同一 wait-queue/RunState 协议上加入 signal interruption、futex 与标准时间 ABI。
- idle 使用 `wfi`；没有 runnable work 时不忙轮询。

## 5. 验收条件

- 生产源码只存在一个 runqueue 实现，无 `dyn Scheduler`、FIFO/Priority 或无调用查询 façade。
- 每个 enqueue/dequeue/current/block/wake/exit 都检查旧 `RunState`，非法转换 fail-stop 或明确忽略 stale wake。
- deadline sleep 不扫描 TGID table；timer softirq 只消费到期 wait queue entry。
- wake-before-switch 不能让同一 task 在两个 hart 同时执行，重复 wake 不重复 enqueue。
- `ready_entries` 与 authoritative logical Ready transition 一一对应；physical stale
  entry 被 pop/retain 时不改变该投影。
- workspace check、三组件构建、ext2 与 8 hart QEMU 启动通过；不运行测试。

## 6. 最终实现

- 删除 `Scheduler` trait、FIFO/Priority 源码和 count/query/all-tasks façade；`Processor` 直接拥有唯一 `CfsRunQueue`。
- `SchedulingState` 在一个 `IrqMutex` 中统一拥有 `RunState`、enqueue generation 和 deadline wait key。Ready entry 同时携带 generation 与不可变 vruntime snapshot；旧 generation 即使仍在 heap/mailbox 也只能被消费丢弃，不能再次执行。
- `Processor::select_task` 在 owner-hart 关中断临界区内完成 dequeue、generation 校验、Ready→Running 与 current 发布；suspend 在同一临界区完成 current 移除、Running→Ready 和 local enqueue。
- deadline wait queue 使用 `(absolute_deadline, sequence)` 唯一 key，不再遍历 TGID table。Running→Blocking 时 state lock 覆盖 queue insertion；remote wake 遇到 Blocking 只写 WakePending，idle switch-return 再完成 Ready enqueue，因此 wake-before-switch 不会双核执行。
- deadline、signal wake 和 SIGCONT 都消费唯一 membership；重复/stale wake 因 key/generation 不匹配而无效。timer softirq 每次最多处理 32 个 entry，路径中不做动态分配。
- timer softirq 只设置 per-hart `need_reschedule`；真正 preemption 在统一用户态返回点消费 flag，hardirq/kernel interrupt 中不直接调度。
- remote wake 先写目标 inbound mailbox，再发 SBI IPI。per-hart `ready_entries`
  在 SchedulingState lock transaction 内投影 logical Ready membership，覆盖 local
  heap 与 inbound mailbox；Relaxed 读取只用于负载/tick projection，不发布 task 状态。
- idle 用关中断的 drain→select→WFI 窗口消除 IPI/WFI lost-wakeup；无 work 时仍使用 `wfi`，不忙轮询。

## 7. 明确不支持的策略

- 当前无 clone/fork 和多 task workload，不实现 work stealing 或周期 migration；new/wake 使用 active CPU 的 queue+mailbox 近似负载选择，保持 last-CPU 缓存亲和性。
- 当前无 sched affinity/实时调度 ABI，不保留 affinity mask、policy enum 或策略切换 façade。
- vruntime runqueue 已支持 timer-driven preemption，但只实现固定权重的最小公平排序；不宣称 Linux CFS 的完整权重、带宽或层级语义。
- Phase 7 将 deadline wait 协议扩展到 signal interruption/futex；Phase 6 不增加虚假的通用 wait-channel API。

## 8. 验证结果

- `git diff --check`、`cargo check --workspace`：通过；kernel warning 从 Phase 5 的 312 降至 306。
- `make build-user`、`make build-kernel`、`make build-bootloader`：通过。
- `python3 create_fs.py create`：成功创建 128 MiB ext2 并写入 `/bin/init`。
- 两次 8-hart QEMU 冷启动（boot hart 4/2）：全部 hart 上线、ext2 挂载、signal 初始化、init 创建并入队；8–10 秒 timer preemption/yield 观察窗口内无 panic、fault 或 current/runqueue invariant 失败。
- static search：生产代码不存在 `TaskStatus`、全表 sleep scan、替代 scheduler、`dyn Scheduler`、未接入 scheduler query、`queued_tasks` 或旧状态更新 façade。
- 心智边界：逐条核对 wake-before-switch、duplicate wake、stale generation、remote mailbox、IPI-before-WFI、timer preemption、stop/continue、exit 与 deferred reap。
- 按仓库规则未执行、维护或修正测试用例。
