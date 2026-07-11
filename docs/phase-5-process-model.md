# LiteOS Phase 5：进程、线程与资源模型

> 审计日期：2026-07-11（Asia/Shanghai）
> 代码基线：提交 `e868bcd`（Phase 0–4）
> 规范基线：[standards-baseline.md](standards-baseline.md) 中固定的 Linux syscall、POSIX.1-2024、riscv64 psABI 与 Rust ownership 资料。
> 验证约束：不维护、不修正、不执行测试；只使用构建、ownership/call-chain 检查和非测试 QEMU 启动观察。

## 1. 阶段范围与当前能力

当前 syscall ABI 只有 `getpid/gettid/execve/exit`，没有 `clone/fork/vfork/exit_group/wait4/set_tid_address/set_robust_list`。因此本阶段建立显式的一进程一线程模型，不伪造 parent/child、thread group、TLS、clear-child-tid 或 robust-futex 语义；这些能力只有在对应标准 ABI 和 Phase 7 futex/signal 基础同时存在时才能加入。

## 2. 统一术语

- **Process**：拥有 TGID、AddressSpace、FileDescriptorTable、cwd、credentials、SignalDisposition 及未来 parent/child/wait 关系的资源容器。当前没有 `chroot` ABI，root 是全局 VFS root，不伪造 per-process 字段。
- **Thread**：拥有 TID、kernel stack、用户寄存器/trap context、TLS、SignalMask/thread-pending 与 clear-child-tid/robust-list 的执行流。当前每个 Process 恰好一个 Thread，故 TID=TGID。
- **Task**：kernel 中可被调度的 Thread 对象，不是 Process 的别名。
- **SchedulingEntity**：Task 内的 run state、CPU/affinity hint、vruntime、runqueue membership 与 sleep deadline；Phase 6 将其收敛为唯一状态事务。
- **PID/TGID**：标识 Process/thread group；当前唯一 init process 的 TGID 为 1。
- **TID**：标识 Thread；单线程模型中等于 TGID，但 API 和 ownership 不应混写。
- **ProcessGroup/Session**：标准 job-control 标识；当前无对应 ABI/状态，不创建占位字段。
- **AddressSpace**：`Mutex<MemorySet>` 保护的 process-owned 映射事务。
- **FileDescriptorTable**：fd 到 `Arc<OpenFileDescription>` 的 process-owned 映射。
- **OpenFileDescription**：当前 `FileDescriptor`；包含共享 inode、offset、status flags，Phase 8 将按 POSIX 命名并修正并发 offset。
- **SignalDisposition**：process-owned handler/action 表；**SignalMask/thread pending** 是 thread-owned。改动前 `SignalState` 错误地把两类状态混在一个 TCB mutex 中。
- **Parent/Child/ThreadGroup**：当前不存在；没有 `fork/clone/wait` 时不得用 zombie 或字段名暗示已实现。

## 3. 改动前 ownership graph

`TaskManager.tasks` 与 per-hart runqueue/current/mailbox 都持有 `Arc<TaskControlBlock>`。TCB 同时内嵌 PID handle、process name、address space façade、kernel stack、trap/task context、fd table、cwd、credentials、signal all-state、exit code、run status、scheduler fields 和 sleep deadline。没有独立 Process owner，也没有 weak parent/child graph。

## 4. 关键调用链

1. boot → `TaskControlBlock::new_with_pid` → ELF/AddressSpace + kernel stack + fd/signal/process state → `TaskManager.tasks` + CFS runqueue。
2. scheduler idle stack → `switch_to_task(Arc<TCB>)` → `__switch` → task kernel stack → syscall/trap → suspend/block/exit → `schedule_with_task_context(Arc<TCB>)` → `__switch` 回 idle。
3. exit → set exit code/zombie → close fds → 再写 zombie → 携带 owning Arc 离开到永不恢复的 task stack；PID table 同时永久持有另一 Arc。
4. exec → prepare new MemorySet → close CLOEXEC/reset all signal state → replace address space/name/trap context；process/thread state没有类型边界。
5. signal → PID table 查 TCB → 同一 `SignalState` 同时修改 handlers、process pending、thread blocked 与 trap flag。

## 5. 当前不变量及缺口

- 一个 Task 不能同时位于 current 和任一 runqueue，但当前 membership 与 `TaskStatus` 不在同一事务；Phase 6 完成。
- context switch 时 raw `TaskContext` 和 kernel stack 必须存活；正常 suspend/block 由 PID table/runqueue/局部 Arc 保活。
- 退出 Task 不能在自己的 kernel stack 上析构；必须先把该栈上的 owner 移交给 processor，再切到其他 stack 释放。并发观察者可能暂持 Arc，因此只要求最终 Drop 不发生在被释放的自身栈上。
- `execve` 只替换当前单线程 Process；没有其他 Thread 可终止。未来加入 clone 前必须实现“exec kills sibling threads”事务。
- 没有 parent/wait ABI 时不存在可被用户观察/消费的 zombie；永久 zombie 不是兼容实现。
- fd table、cwd、credentials、signal dispositions 和 address space 是 process-owned；kernel stack、trap/task context、mask/pending 和 scheduling fields 是 thread-owned。

## 6. 已确认问题

| 严重度 | 问题 | 直接后果 |
|---|---|---|
| Blocker | exit 路径的 owning Arc 永久停留在退出 task kernel stack，PID table 也不移除 | kernel stack、AddressSpace、TCB、TGID 永久泄漏 |
| Critical | 无 parent/wait 消费者却保留 Zombie | 退出状态无权威消费者；任务表无限增长 |
| Critical | TCB 混合 process/thread/scheduling ownership | exec、未来 clone、signal sharing 与资源释放无法证明 |
| Critical | `SignalState` 混合 dispositions、blocked mask、pending 与 trap flag | exec 错误清空 thread mask；无法表达 thread/process signal sharing |
| Major | `PidHandle` 只是整数 wrapper，无 allocator/Drop/回收协议 | 一旦增加进程创建会重复或泄漏 ID |
| Major | exit cleanup 先设 zombie、close fd、调用方又设 zombie | 双轨状态写与重复 cleanup 入口 |
| Major | process name、exit code、credentials/cwd/fd table散落在 TCB | process resource teardown 不能作为单一 owner Drop/commit |
| Major | scheduler 还暴露 FIFO/Priority 替代实现与查询 façade | Task/SchedulingEntity 模型有多套装饰性结构；Phase 6 删除 |

## 7. 对应标准

- Linux：`getpid` 返回 TGID，`gettid` 返回 TID；单线程时数值可相等但概念不同。`execve` 保留 PID、fd table 中非 CLOEXEC 项、cwd/credentials 并重置信号 dispositions，不重置信号 mask。
- POSIX：fd table entry 指向可共享 open file description；cwd/credentials/dispositions 属于 process，signal mask 属于 thread。
- Rust ownership：对象必须由明确 owner Drop；在对象自己的 stack 上释放 kernel stack 是 use-after-free。跨 switch raw pointer 必须由另一个栈上的 owner 保活。
- Linux exit/wait：zombie 只为 parent wait 保存最小退出信息；没有 parent/wait 模型时不得把完整 TCB 当永久 zombie cache。

## 8. 目标模型

- `Process` 独占 TGID 与 process resources；`TaskControlBlock` 表示唯一 Thread/SchedulingEntity，并直接拥有 `Process`。当前没有第二个 Thread，不为未来 sharing 预留多余 `Arc`。
- `ThreadContext` 独占 kernel stack、trap context VA 与 `TaskContext`；AddressSpace 移到 Process。
- process signal dispositions 与 thread signal state 分开；当前一对一映射不引入 thread group vector。
- exit 先从 TGID index/current 移除 task，再使用 per-hart deferred-reap slot 把 task-stack owner 转移给 current hart；切回 idle 后释放 deferred owner，`switch_to_task` 的 idle-stack owner随后释放。若并发观察者暂持 Arc，最终 Drop 可稍后发生，但绝不会发生在被释放 task 的自身栈上。
- 当前无 wait ABI，exit 后立即回收完整 Process/Thread；不保留 Zombie。未来 wait4 需要独立最小 `ChildExitRecord`，不能复活完整 TCB zombie。
- TGID/TID API 分开命名；当前 ID=1 静态分配，新增 creation ABI 前必须先实现 RAII allocator。

## 9. 删除项

- 无消费者的完整 TCB zombie retention 与重复 zombie 写。
- 无调用的 PID 任意 `From<usize>` façade（init 使用显式构造）。
- TCB 上 process resource 的 public 直达字段，改由 `Process` 小接口访问。
- 重复 signal reset API；exec 只把捕获型 handler 重置为默认，保留 `SIG_IGN`、thread mask 与 pending。
- Phase 5 审计确认无调用的 task/process wrapper；调度器替代实现留到 Phase 6 与唯一 scheduler 一并删除。

## 10. 修改计划

1. 引入 `Process`/`ThreadContext` ownership，迁移 address space、fd、cwd、credentials、name、exit/signal owner → verify: 每个字段只属于一个语义 owner。
2. 分离 TGID/TID API 与 process/thread signal state → verify: exec reset dispositions但保留 mask，kill lookup 使用 TGID。
3. 实现 PID table remove + per-hart deferred reap → verify: 最后一个 Arc 只在 idle stack Drop，exit task 不再留在任何索引/queue/current。
4. 收敛 exec/exit cleanup 顺序与错误边界 → verify: prepare 失败不改 process；commit 后无可恢复失败；fd/MemorySet恰好释放一次。
5. 删除无 ABI 支撑的 façade并记录 parent/child/thread-group defer → verify: 不存在伪 zombie/wait/clone 状态。
6. 构建、ownership/static search 与 8-hart QEMU 观察 → verify: init 连续 yield；不运行测试。

## 11. 验证方式

- `git diff --check`、`cargo check --workspace`、三组件构建、定向 rustfmt。
- 列出所有 `Arc<TaskControlBlock>` owner 与 context-switch 前后 Drop 栈；检索 TaskManager/runqueue/current/inbound/deferred slot membership。
- 检索 PID/TID/TGID、process resource 直达字段、Zombie、signal reset、CLOEXEC 和 AddressSpace lock。
- 心智验收 init exec、exec prepare failure、CLOEXEC、exit from syscall/signal、last task exit、remote signal 与 deferred Drop。
- QEMU `virt -smp 8` 非测试启动观察；仓库规则禁止执行、维护或修正测试。

## 12. 风险与阶段边界

- Phase 6 才统一 run state/current/runqueue membership；本阶段 deferred reap 只处理 terminal ownership，不能假装完成调度事务。
- Phase 7 完成标准 signal syscall、process-directed selection 与 futex/robust-list；本阶段只纠正 process/thread state ownership。
- Phase 8 将 `FileDescriptor` 正名为 OpenFileDescription 并实现 sleepable offset serialization；本阶段只移动 owner，不在 spin lock 内包 I/O。
- 增加 fork/clone/wait4 前必须扩展 Process thread/child collections、weak parent、ID allocator 和 wait queue；本阶段明确不创建占位容器。

## 13. 完成后的 ownership graph 与结果

`TaskControlBlock` 现在只组合三个语义 owner：

1. `Process`：TGID、AddressSpace、FileDescriptorTable、cwd、credentials、SignalDispositions；由当前唯一 Thread 的 TCB 直接拥有。
2. `ThreadContext`：TID、kernel stack、trap context VA、`TaskContext`、thread pending/mask。
3. `SchedulingEntity`：run state、调度策略字段、last-CPU hint、sleep deadline、stop/resume 状态。

TGID index、per-hart runqueue/mailbox/current、idle-stack `switch_to_task` 局部 owner、短期查询结果和 deferred-reap slot 是全部 `Arc<TaskControlBlock>` owner。普通 suspend/block 在 `__switch` 前释放 task-stack Arc，由 TGID index 与 runqueue/sleep owner 保活 raw context；syscall、sigreturn 和 fatal-signal trap 在进入不返回的 exit 前也显式释放局部查询 Arc。exit 路径将状态置为 `Exited`，删除 TGID index owner，把自身 owner移入 deferred slot并切回 idle；完整 TCB 不作为 zombie 保留。当前没有 wait 消费者，因此 exit code 不进入伪缓存；未来 wait4 必须引入独立的最小 `ChildExitRecord`。

`getpid` 明确返回 TGID，`gettid` 明确返回 TID，kill 只按 TGID index 查找。当前两者数值均为 1，但接口不再互相代用。`ProcessId::init()` 是唯一 ID 构造入口；在加入进程/线程创建 ABI 前，不开放任意整数构造或虚假回收协议。

exec 在提交前完整构造新 AddressSpace；准备失败不修改 Process。提交时只关闭 CLOEXEC fd、重置捕获型 signal handler、替换 AddressSpace/name/trap context；`SIG_IGN`、thread signal mask/pending、TGID/TID、cwd 与 credentials 保留。旧 AddressSpace 与 fd table 都由 Process owner 恰好析构一次。

## 14. 验证结果

- `git diff --check`：通过。
- `cargo check --workspace`：通过；kernel 有 312 个 dead-code/unused 类 warning，Phase 6 将删除未接入的替代调度器与查询 façade。
- `make build-user`、`make build-kernel`、`make build-bootloader`：均通过。
- `python3 create_fs.py create`：成功创建 128 MiB ext2 镜像并写入 `/bin/init`。
- 两次 `qemu-system-riscv64 -machine virt -smp 8` 冷启动：分别由 boot hart 0/5 启动，8 个 hart 全部上线，ext2 成功挂载，signal subsystem 初始化，`init task created and queued`；观察窗口内无 panic/fault。
- ownership/static search：生产代码中不存在 `Zombie`、`PidHandle`、`SignalState`、`find_task_by_pid`、混用的 `pid()` 或重复 exit cleanup；Process/Thread/SchedulingEntity 字段访问均落在对应边界。
- 按仓库规则未执行、维护或修正测试用例。
