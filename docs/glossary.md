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
| Audio output | 从 Web runtime 产生音频帧，经系统音频服务与 kernel 设备 seam 播放到 host 的单向能力。 |
| Audio capture | 从 microphone 等输入设备采集音频帧并交给 Web runtime 的能力；不属于首个声音里程碑。 |
| Media element | LiteUI 对标准 `<audio>` / `HTMLMediaElement` 播放状态、控制方法和事件的实现。 |
| Web Audio | 以 `AudioContext` 和音频节点图提供实时合成、处理与精确调度的 Web API；不属于首个声音里程碑。 |
| Native file capability | 桌面应用用于发现、授权和打开本地文件的 `lite:fs` 能力；不拥有媒体解码或播放状态。 |
| Filesystem-backed File | 由 Native 文件能力授权并惰性读取的标准 `File` 对象；其内容不整体进入 QuickJS heap。 |
| Logical playback stream | 一个 media element 提交给系统 mixer 的独立播放序列；不直接拥有物理 PCM 设备。 |
| System audio service | 独占物理 PCM output、设备时钟、混音、格式归一化与 underrun 恢复的系统进程。 |
| ALSA PCM playback | LiteOS kernel 向 Native userspace 发布的 Linux-compatible PCM 输出 ABI；不暴露 VirtIO adapter。 |
| VirtIO Sound adapter | kernel 内独占 VirtIO Sound queues、DMA、command lifecycle、completion 与 reset 的具体设备实现。 |
| Audio worker | 每个 LiteUI 进程中唯一拥有媒体读取、解码、seek、PCM 排队和音频服务 IPC 的非 UI 线程。 |
| Transparent playback | 除必要格式转换、用户 gain 与 overload 保护外不主动改变波形的共享播放目标；不等同 bit-perfect。 |
| PCM normal form | audio worker、system mixer 与设备边界共同使用的 48 kHz stereo interleaved `f32` 格式。 |
| Codec baseline | media element 通过 Symphonia 0.6.0 可以真实声明并完整解码的全部稳定音频格式集合。 |
| Playback grant | 一次真实用户激活授予当前 app process 的有声播放许可；进程退出即撤销。 |
| PCM ring | 每条逻辑 stream 由 memfd 提供、audio worker 单写且 system mixer 单读的固定容量共享环。 |
| Render block | audio worker 产生的 128-frame PCM 工作单元；mixer 每个 256-frame device period 消费两个。 |
| Confirmed media time | system mixer 已确认消费的最后 PCM frame 所对应时间；服务失败时 `currentTime` 回落到该位置。 |
| Master volume | system audio service 在最终 mix output 唯一应用的系统级 gain/mute；不同于 element volume。 |
| Linear element gain | `HTMLMediaElement.volume` 的标准 `0..1` amplitude multiplier；不同于 master slider 感知曲线。 |
| Overload limiter | 仅在混合峰值将超过 1.0 时介入的 128-frame lookahead 保护；不是常驻 compressor。 |
| Live playback stream | 已申请 decoder/ring/mixer identity 的 media element stream；paused 时仍属于 live。 |
| Audio domain | 从 VirtIO adapter、kernel ALSA PCM、system mixer 到 LiteUI media adapter 的独立 owner 链。 |
| Audio control connection | 每个 LiteUI audio worker 用于管理最多 8 条 stream 的唯一 AF_UNIX connection。 |
| Music Player | 只组合 `lite:fs`、filesystem-backed `File` 与标准 media element 的 production 验收应用。 |
