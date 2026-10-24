# LiteOS 术语表

## 架构与内核

文中的源码路径相对 `kernel/src/`。

| 术语 | 定义 |
|---|---|
| Owner（所有者） | 某份复合状态的唯一维护者：只有它能修改这份状态，其他模块只能经它的接口读写。全局状态必须在声明旁写 `OWNER:` 说明归属，缺失会被 architecture-check 拒绝。例：`memory/mod.rs` 的 `KERNEL_SPACE`。 |
| Façade（门面） | `crate::arch` 与 `crate::platform`：编译期按目标架构选定后端并统一导出的模块。通用内核只经它们访问硬件，没有 trait 对象和运行时分派。见 `arch/mod.rs`。 |
| Seam（接缝） | 两层之间刻意保持很窄的接口，后端在这里插入。例：`hal::console::SerialOutput`，platform 提供输出函数，tty 只依赖这个接口。 |
| Adapter | 实现某个 seam 的具体设备或后端，例如 VirtIO 网卡、PL011 串口。通用代码不得依赖具体 adapter 的类型。 |
| Publication（发布） | 让一份新状态对其他 CPU、线程或其他 owner 可见的那一刻：插入注册表、挂进进程图、写入页表、放入就绪队列。发布之前的失败可以直接丢弃；发布之后只能按协议回滚。 |
| Prepare / Commit（准备 / 提交） | 把一次更新拆成两步：准备阶段做所有可能失败的事（分配、校验），提交阶段只做不会失败的发布。例：`fallible_tree` 的 `try_prepare` 与 `commit_vacant`。 |
| Token（令牌） | 证明某个前置条件已经满足、通常只能消费一次的值。例：启动顺序令牌（`SchedulerReady`、`RootMounted`、`ConsoleReady`，顺序错误无法编译）、预分配节点 `VacantEntry`、就绪转换 `ReadyTransition`。 |
| Generation（代际） | 单调递增的编号，用来让旧令牌或旧队列项失效，而不必去找出并删除它们。例：就绪队列里 `RunQueueEntry` 的 generation。 |
| Projection（投影） | 由权威状态在同一把锁或同一事务里派生出的汇总值，用于快速读取，例如空闲页数、各 CPU 的就绪数。它不是第二份独立状态；“复制状态并人工同步”被项目规则禁止。例：`frame_allocator.rs` 的 `free_pages`。 |
| Fail-stop（失败即停） | 内核发现不变量被破坏时 panic，并经 platform 重置整个系统，而不是带着已经损坏的状态继续运行。见 `lang_item.rs`。 |
| Fence（围栏） | 有两个含义：一是硬件层面，让一次地址翻译修改对所有 CPU 生效的屏障或 TLB 刷新（例：`memory/mm/shootdown.rs` 的 `TranslationCommit`）；二是 architecture-check 里的静态检查项，输出 “architecture fence passed/failed”。 |
| Gate（门禁） | 一项必须通过的验证步骤，例如 `make verify-runtime-boot`。runtime gate 会启动 guest 并解析它的输出。 |
| Contract（契约） | `docs/architecture-contract*` 里对某个领域的 module、接口、依赖与状态 owner 的权威约定。接口变更必须同步更新契约和 `docs/generated/architecture-interface.txt`，由 architecture-check 对照检查。 |
| Hardirq / Deferred work / Safe point | Hardirq 是硬件中断处理函数，只确认设备并置位；Deferred work 是它置下的 per-CPU 位，真正的工作稍后执行（类似 softirq）；Safe point 是允许运行延迟工作的位置，如返回用户态前。见 `trap/mod.rs`。 |
| IrqMutex / TaskMutex | `IrqMutex` 先关本地中断再自旋，用于中断路径也会访问的数据，临界区内不得睡眠；`TaskMutex` 只能在 task 上下文使用，等待时睡眠，用于需要跨可睡眠 I/O 持有的状态。见 `sync/mod.rs`。 |
| RAII owner | 用 Rust 值的生命周期持有资源、drop 时归还，例如 `FrameTracker`（物理页区间）。 |
| Buddy order | 伙伴系统中块大小 2ⁿ 页的指数 n。order n 的块只有一个伙伴，页号为 `B ^ (1 << n)`。见 `memory/frame_allocator.rs`。 |
| vruntime | 虚拟运行时间。调度器总是运行 vruntime 最小的任务；运行时间按权重折算后累加，nice 越小涨得越慢。见 `task/scheduler/cfs_scheduler.rs`。 |
| PPN / VPN | 物理页号 / 虚拟页号：地址除以页大小。 |
| VMA | Virtual Memory Area：进程地址空间里一段权限与后备相同的连续区域。 |
| COW | Copy-on-write：fork 后父子共享只读页，第一次写入时才复制。 |
| TLB shootdown | 映射被修改后，其他 CPU 缓存的旧翻译必须失效：发起方经 IPI 通知它们，并等待确认后才能释放被撤销的页。 |
| ADR | Architecture Decision Record：`docs/decisions/` 里记录一项决策的背景、结论与后果。 |

## 音频

| 术语 | 定义 |
|---|---|
| Audio output | 用户态向 ALSA PCM 写入音频帧，由内核混音后经设备 seam 播放到 host 的单向能力。 |
| Audio capture | 从 microphone 等设备采集音频帧；当前不支持。 |
| PCM substream | 每个 ALSA playback open 独占的状态、应用指针和环形缓冲，关闭后归还子流槽位。 |
| Master | 卡级左右声道音量与开关，由 ALSA control UAPI 访问，内核混音引擎独占状态。 |
| Period | 硬件提交与完成的记账单位；当前为 256 帧、48 kHz、双声道 F32_LE。 |
