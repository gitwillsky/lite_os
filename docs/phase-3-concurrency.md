# LiteOS Phase 3：同步原语与内核并发模型

> 审计日期：2026-07-11（Asia/Shanghai）
> 代码基线：Phase 1、Phase 2 未提交工作树
> 规范基线：[standards-baseline.md](standards-baseline.md) 中固定的 Rust、RISC-V ISA/Privileged Architecture 与 SBI 一手资料。
> 验证约束：不维护、不修正、不执行测试；只使用构建、源码/锁序/反汇编检查和非测试 QEMU 启动观察。

## 1. 阶段范围

本阶段在继续内存、进程和调度重构前建立同步基础：本地中断屏蔽的 RAII 生命周期、interrupt-safe spin mutex/rwlock、普通 task-context spin lock 的适用边界、per-hart 独占、原子发布、锁顺序和中断上下文限制。本阶段不实现 sleep mutex、wait queue 或抢占调度；当前没有可证明正确的阻塞原语，Phase 6 才在统一 task state 后建立 wait queue。

## 2. 当前实现

- kernel 没有 `sync` 模块。logger、console、allocator、任务表、TCB、PLIC、设备、VFS 和文件系统均直接使用 `spin::Mutex/RwLock`，类型不表达能否在中断上下文获取。
- S-mode trap 关闭 SIE 且不嵌套，但普通 kernel 路径通常保持 SIE；如果普通路径持锁时被中断，中断 handler 仍可能获取相同锁。
- PLIC 注册路径和 device manager 各自手写 `read SIE → clear SIE → lock → drop → restore SIE`，其他 interrupt-shared 锁没有同等保护。
- timer hardirq 置 SSIP；softirq 在 trap 中扫描全局任务表、创建 `Vec<Arc<TCB>>`、修改 task status 并投递 runqueue/mailbox。
- allocator 由 `LockedHeap` 和九个 `spin::Mutex<SlabCache>` 组成；分配期间不屏蔽本地中断。
- context switch 前存在两个无配对同步对象的 `SeqCst fence`；softirq 在 `fetch_or(AcqRel)` 后又执行独立 Release fence。

## 3. 关键调用链

1. task context 持 logger/console lock → timer/external interrupt → interrupt handler 再次 log → 同 hart 自旋死锁。
2. task context 进入 global allocator → timer SSIP → task table scan/`Vec`/runqueue insertion → global allocator 再入 → 同 hart allocator lock 死锁。
3. S external interrupt → `device_manager` lock → interrupt-controller lock → PLIC handler/statistics lock → device MMIO acknowledgement。
4. S timer/soft interrupt → task-table read lock → task-status lock → status transition → per-hart scheduler 或 remote mailbox lock。
5. syscall/task lifecycle → fd-table lock → 删除最后一个 `Arc<FileDescriptor>` → `Drop::drop` 调用 inode `sync()`，把潜在文件 I/O 隐藏在 spin-lock 生命周期内。
6. task suspend/block → 修改 status/runqueue → 无关联 `SeqCst fence` → `__switch`；屏障不建立状态与队列的一致性。

## 4. 关键数据结构

- 普通共享锁：`spin::Mutex<T>`、`spin::RwLock<T>`、`spin::Once<T>`。
- interrupt-shared 数据：logger、console、global allocator、`TaskManager.tasks`、TCB status、per-hart inbound mailbox、PLIC handler/statistics 与 controller ownership。
- per-hart 独占：Phase 2 的 `PerHartProcessor.local`，只由 owner hart 在 SIE 关闭时访问；远端只接触 atomics 与 inbound queue。
- 发布原子：boot/global init、online mask、RFENCE request/ack、timer interval；这些已有明确 Release/Acquire 配对。
- 非发布原子：CPU time/accounting、fd offset、heap cursor、signal bitmap、DMA/virtqueue index；其领域语义分别留在 Phase 4/5/7/8/10，但不能被误当作复合状态事务。

## 5. 当前不变量及证据

- `spin::Mutex` 能阻止多个 hart 同时进入临界区，但不会屏蔽本地中断；因此它只能用于中断路径永不触及的数据，当前类型和调用点没有表达该限制。
- S-mode trap handler 内 SIE 为关闭状态；interrupt-safe lock 的 task-context 获取侧必须先 clear SIE，释放底层 lock 后才恢复原状态。嵌套获取时内层不得提前打开中断。
- context switch 时不得持有任何 lock guard；Phase 2 已禁止 `&mut Processor` 跨 switch，但 task context raw pointer 与状态/runqueue 事务仍留给 Phase 6。
- interrupt handler 不得执行可能睡眠的 I/O；目前 VirtIO block handler 只做 MMIO ack，但 logger、分配器和 task wakeup 仍可能自旋。
- `Arc` 只证明对象存活，不保护对象内部状态，也不证明最后一个引用的 destructor 可在当前锁/中断上下文执行。

## 6. 已确认问题

| 严重度 | 问题 | 直接后果 |
|---|---|---|
| Blocker | logger/console 与 allocator 使用普通 spin lock，却可从中断上下文重入 | 同 hart 在自己持有的锁上永久自旋，系统无进展 |
| Critical | task table/status/mailbox 和 PLIC/controller 同时由 task 与 interrupt context 获取，类型不屏蔽本地 SIE | 任一普通路径被相应中断打断时可自死锁 |
| Critical | PLIC/device manager 手工复制 SIE 保存恢复 | early return、panic 或未来分支可漏恢复；嵌套规则没有统一证明 |
| Critical | `FileDescriptor::Drop` 隐式执行 inode `sync()` | 最后一个 Arc 在 fd-table 或其他 spin lock 内销毁时发生锁内 I/O和反向锁序 |
| Major | timer softirq 在中断上下文分配 `Vec` 并扫描全任务表 | allocator 重入风险、O(tasks) 中断延迟；后者最终由 Phase 6 timer queue 删除 |
| Major | context switch 前两个孤立 `SeqCst fence` | 不与任何原子协议配对，不能修复 status/runqueue 多权威，只制造错误安全感 |
| Major | softirq `fetch_or(AcqRel)` 后再做 Release fence | 冗余且没有新的写需要发布，模糊真正的 request/consume 协议 |
| Major | interrupt HAL 保留未使用的 WorkQueue/BottomHalf/SoftIrqManager/Basic controller | 形成装饰性同步模型和死代码，无法回答所有权或执行上下文 |
| Major | fd offset 使用 `load → I/O → fetch_add` | 共享 open-file-description 并发 I/O 可使用同一旧 offset；Phase 8 需 sleepable OFD serialization |
| Critical | `Memory::trap_context()` 在页表锁释放后返回 `'static mut` | Arc/lock 不证明映射生命周期或别名唯一性；Phase 4 必须移除该用户内存边界 |

## 7. 对应标准

- Rust Reference/Rustonomicon：`MutexGuard` 生命周期必须覆盖被保护访问；裸指针和 `Arc` 不解除 aliasing、Send/Sync 或 destructor-context 义务。
- Rust atomics：Release/Acquire 只通过同一原子上的 read-from/修改序列建立 happens-before；孤立 fence 不能替代状态所有权和锁事务。
- RISC-V Privileged Architecture：SIE 是当前 hart 的 supervisor interrupt enable；interrupt-safe 临界区必须保存原值、先关闭、释放锁后恢复，不能无条件打开。
- 内核执行上下文约束：hardirq/softirq 不得睡眠；spin lock 内不得调度或执行潜在阻塞 I/O；同 hart 可重入资源必须在获取锁前屏蔽对应中断。

## 8. 目标模型

- `LocalIrqGuard` 只管理当前 hart 的 SIE，构造时保存并 clear，Drop 时仅在原值为 enabled 时恢复；嵌套 guard 自然保持关闭。
- `IrqMutex<T>`/`IrqRwLock<T>` 组合 local IRQ guard 与 `spin` lock，guard Drop 明确先释放底层锁、再恢复 SIE。它们用于 task/interrupt 共享数据。
- 普通 `spin::Mutex/RwLock` 只用于中断路径不可达、临界区短且不调度/不做 I/O的数据；模块文档记录这一限制。
- global allocator 每次 alloc/dealloc 的整个内部锁路径由 `LocalIrqGuard` 包围，防止 timer/external interrupt 在同 hart 重入 buddy/slab/frame 分配路径。
- PLIC、logger/console、任务表/status/mailbox 等 interrupt-shared 状态迁移到 IRQ-safe lock；删除散落的手工 SIE 管理。
- 删除无配对 fence、隐藏 I/O destructor 和未接入的替代同步抽象。Phase 6 再以唯一 task state + wait queue 替换 timer 全表扫描。

## 9. 删除项

- `FileDescriptor::Drop` 中的隐式 inode sync 与只服务该行为的 dirty flag。
- suspend/block 前无同步对象的 `SeqCst fence`。
- softirq `fetch_or(AcqRel)` 后的冗余 Release fence。
- interrupt HAL 中无调用者的 WorkQueue、WorkItem、BottomHalf、SoftIrqManager、BasicInterruptController 与 SimpleInterruptHandler。
- device manager/PLIC 中重复的手工 SIE 保存恢复分支。

## 10. 修改计划

1. 实现 `LocalIrqGuard`、`IrqMutex`、`IrqRwLock` → verify: guard Drop 先 unlock 后 restore，嵌套获取不提前开中断。
2. 迁移 logger、console、allocator、task timer path、processor mailbox、device manager 与 PLIC interrupt-shared 锁 → verify: 相应路径不再直接使用裸 `spin` lock或手写 SIE 对。
3. 删除隐藏 I/O destructor、孤立 fence 与未使用同步抽象 → verify: spin guard 内无已知调度/文件 I/O，源码无装饰性状态模型。
4. 审计剩余 atomics/locks 并记录 defer 边界 → verify: Release/Acquire 有配对；Relaxed 只用于统计、hint 或后续阶段明确重构对象。
5. 执行 workspace/组件构建、源码检索、反汇编与 8-hart QEMU 观察 → verify: init 创建且观察窗口无 panic/deadlock；不运行测试。

## 11. 验证方式

- `git diff --check`、`cargo check --workspace`、`make build-kernel`、`make build-bootloader`、`make build-user`。
- 检索所有 `spin::Mutex/RwLock`、手工 SIE、Relaxed、SeqCst fence、lock 后 schedule/I/O 调用，逐项分类。
- 反汇编确认 `LocalIrqGuard`/IRQ lock 获取路径在 lock 前清 SIE，Drop 路径在 unlock 后条件恢复。
- 心智验收 nested irq guard、task-context 被 timer/external interrupt 打断、跨 hart 竞争、panic/early return、softirq wakeup 与 context switch。
- QEMU `virt -smp 8` 非测试启动观察，记录随机 cold-boot hart、init 创建与稳定运行。

## 12. 风险

- IRQ-safe spin lock 只消除同 hart interrupt reentrancy，不允许 lock 内睡眠，也不解决跨锁死锁；每个迁移点仍需明确锁序。
- timer softirq 的全表扫描、分配和 status/runqueue 非原子事务属于 Phase 6 的结构性问题；本阶段先保证其当前执行路径不会重入 allocator/共享锁自死锁。
- fd offset 需要可睡眠的 open-file-description serialization，不能用 IRQ spin lock 包住文件 I/O；留给 Phase 8 与 wait queue 一起完成。
- signal bitmap、虚拟内存、VirtIO/DMA atomics 的完整领域语义分别属于 Phase 7、4、10；本阶段只禁止把它们误用作跨对象发布屏障。
- 当前不实现 lockdep；锁序通过小接口、短临界区、源码清单和阶段文档维持。

## 13. 完成结论

Phase 3 已于 2026-07-11 完成。kernel 现在显式区分 interrupt-shared IRQ-safe spin lock 与 interrupt-inaccessible 普通 spin lock；所有当前 hardirq/softirq 可达的共享锁和 allocator 路径均已按该模型收口。本阶段没有新增或修改用户 syscall ABI。

### 13.1 实际同步模型

| 顺序/类别 | 保护对象 | 可用上下文 | 临界区约束与证据 |
|---|---|---|---|
| `LOGGER → CONSOLE` | logger filter/config、串行 DBCN 输出 | task、hardirq、softirq | 两者均为 `IrqMutex`；只做固定数组查询、格式化和 polling DBCN，不调度、不分配 |
| `TASKS(read) → TASK_STATUS` | PID 索引扫描、单 task status | task、timer softirq | `IrqRwLock → IrqMutex`；`set_task_status` 在反向查 PID 前先释放 status，避免 `status → tasks` 锁环 |
| `TASK_STATUS → local scheduler` 或 `remote INBOUND` | 当前状态转换后的 runnable 投递 | task、softirq | status guard 在入队前释放；local scheduler 由 owner hart + `LocalIrqGuard` 独占，remote queue 为 `IrqMutex` |
| `DEVICE_MANAGER`（短取引用）→ release → `INTERRUPT_CONTROLLER` | 全局设备索引、唯一 PLIC 实例 | boot/task、external interrupt | 两把锁不嵌套；PLIC handler map 是外层 controller guard 内的普通 `BTreeMap`，不存在第二把内部锁 |
| `LocalIrqGuard → SLAB cache → FRAME_ALLOCATOR` | global heap、小对象 cache、physical frame | task、interrupt | global alloc/dealloc 全路径先关 SIE；frame recycler 固定容量且 frame lock 内不分配，因此不存在原 `frame → slab → frame` 反向路径 |
| 普通 task-only spin lock | address space、fd table、signal state、cwd、VFS/FS、block queue、RTC、SBI input byte | 当前已证明的非中断调用点 | 中断 handler 不访问这些锁；禁止持 guard 调度。FS/driver 的领域锁序和 blocking 语义仍分别由 Phase 8/10 完成 |
| bootloader `UART`、`RFENCE_LOCK` | M-mode console、同步 RFENCE 单槽协议 | M trap，MIE 关闭 | M trap 不嵌套；RFENCE 等待循环主动服务当前 hart request，不持其他 spin lock |

`LocalIrqGuard` 带非 Send marker，构造时保存并 clear 当前 hart SIE；嵌套 guard 看到 disabled 状态，不会提前恢复。`IrqMutexGuard`、`IrqRwLockReadGuard` 和 `IrqRwLockWriteGuard` 的 Drop 显式先销毁底层 spin guard，再销毁 IRQ guard。

### 13.2 删除与迁移

- 新增 `kernel::sync::{LocalIrqGuard, IrqMutex, IrqRwLock}`，并迁移 logger、console、global/frame allocator、PID task table、task status、per-hart inbound、device manager 与 PLIC controller。
- global allocator 的 alloc/dealloc 全路径由 local IRQ guard 包围；frame allocator 的 8192-entry `Vec` 预留方案替换为覆盖当前 128 MiB QEMU target 的 32768-entry 固定 PPN 回收栈，启动时强制验证容量。
- timer hardirq 在 user/kernel trap 中统一只登记 softirq；softirq 用 32-entry 栈上 batch 扫描到期 task，不再分配返回 `Vec`。全表扫描本身留待 Phase 6 timer queue 替换。
- PLIC 收敛为一个外层 controller lock；`handle_pending_interrupts()` 依次执行 claim → clone handler → handler MMIO ack → complete，不再分配 vector、提前 complete 或持 handler-map lock执行 callback。
- signal pending/blocked/trap flags 从 mutex 内的冗余 atomics 迁回同一 `SignalState` 普通字段；跨核空消息队列、PID map、active flag 和只会清空消息的 consumer 删除，保留 per-hart `CURRENT_PID` Release/Acquire hint 只用于发送检查 IPI。
- 删除 `FileDescriptor::Drop` 的隐式 inode sync/dirty flag，避免任意最后一个 Arc 在 spin lock 内触发文件 I/O；标准持久化语义由 Phase 8 明确接口完成。
- 删除无配对的 context-switch `SeqCst fence`、softirq 冗余 Release fence、散落的手工 SIE 保存恢复，以及未接入的 WorkQueue、WorkItem、BottomHalf、SoftIrqManager、BasicInterruptController、SimpleInterruptHandler。
- 删除已失效的 init-task cache、runtime scheduling-policy façade、同步状态扫描 wrapper、TCB user/kernel CPU 统计、creation timestamp、`in_kernel_mode` lock 和未使用 priority/time-slice 字段。PID 索引不再虚假声明为全部 task state 的“唯一权威”。
- credentials 的 uid/euid 检查更新和 exit code 由单一 mutex 保护；CFS `last_runtime` 回到 sched mutex，只有负载/CPU hint 继续使用 Relaxed atomic。

### 13.3 原子审计结论

- boot/global init、online mask、timer interval、RFENCE request/ack、processor active、softirq pending、sleep deadline 与 current-PID hint 均有明确 Release/Acquire 或 AcqRel 配对。
- 剩余 Relaxed 分为三类：只提供调度选择 hint 的 `NEXT_CPU/last_cpu/queued_tasks`；不发布对象的 ext2/fat32计数器；已明确交给 Phase 4/8/10 重构的 brk cursor、open-file offset 与 DMA virtual-address cursor。
- `FileDescriptor.offset` 的 `load → I/O → fetch_add` 已记录为不正确的并发 open-file-description 事务，不能因使用 atomic 而视为完成；Phase 8 需要可睡眠 serialization。
- `Memory.heap_base/heap_top` 不是完整 address-space transaction；Phase 4 将其并入统一地址空间锁。

### 13.4 验证结果

- `git diff --check`、`cargo check --workspace`、`make build-kernel`、`make build-bootloader`、`make build-user` 全部成功。最终告警计数为 kernel 330、bootloader 19、user 3；Phase 2 kernel 基线为 370。
- 本阶段涉及的 18 个 Rust 源文件通过定向 `rustfmt +nightly-2025-06-15 --edition 2024 --check`；未执行全仓无关格式化。
- 源码检索确认手工 SIE 修改只剩 boot enable、panic fail-stop、trap return 和 `LocalIrqGuard`；未发现已删除的替代同步类型、空 signal message queue、dirty-on-close 或无配对 context-switch fence。
- `llvm-nm/llvm-objdump` 确认 IRQ rwlock acquire 先 `csrc sstatus` 再 `amo*.aq`；mutex/rwlock drop 先执行 Release unlock，再按保存位条件 `csrs sstatus`。`wake_expired_tasks` 产物使用 32-entry 栈上 batch，无 heap-backed `Vec` 构造。
- 最终 QEMU `virt -smp 8` 非测试观察中 cold-boot hart 为 2，hart mask 为 `0xff`，其余 7 个 hart 全部启动；kernel owner 为 hart 0，完成 allocator、PLIC、VirtIO block、ext2 和 signal 初始化并创建 init task。继续运行 10 秒无 panic、死锁或异常输出，随后正常终止 QEMU。
- 中途观察还覆盖 cold-boot hart 6、kernel owner hart 1，结果一致，说明 kernel 全局 owner 不依赖 CPU0；这仍只是启动路径证据，不替代 Phase 4/6/8/10 的领域并发证明。
- 遵守仓库规则，未执行、维护或修正任何测试。

### 13.5 剩余风险与下一阶段

- fixed frame recycler 依据当前 QEMU 128 MiB 上限；Phase 4 必须把目标物理内存上限纳入显式配置/所有权模型，并审计 frame 回收复杂度。
- `Memory::trap_context()` 仍在页表锁释放后返回 `'static mut`，brk 使用多个原子 cursor 且页表修改事务不完整；这是 Phase 4 的首要 blocker。
- task status、current 与 runqueue membership 仍未成为单一状态事务，timer 仍 O(tasks) 扫描；Phase 6 建立唯一 run state、wait queue 和 timer queue。
- ext2/FAT32 cache/inode lock 可能覆盖同步 block I/O，当前 synchronous polling 路径不会调度，但 POSIX/OFD/阻塞锁序尚未证明；Phase 8 必须重审，不能直接升级为 sleep lock而隐藏问题。
- PLIC context/hart 映射、VirtIO ring/DMA barrier 与设备回调生命周期由 Phase 10 完成；本阶段只证明当前 block IRQ ack 路径的同步边界。
