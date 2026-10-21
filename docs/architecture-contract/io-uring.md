# io_uring 契约

## Owner 与依赖

| Owner | 唯一职责 |
|---|---|
| `kernel::io_uring::abi` | Linux v7.1 UAPI 布局常量、`io_uring_params` 校验与编解码、SQE/CQE/probe 编码；无状态、无 IO |
| `kernel::io_uring::ring` | 共享内存 ring、SQ 消费、CQ 发布、溢出链表、readiness 通知与等待 |
| `kernel::io_uring::ops` | SQE 的 init/prep/issue；支持的 opcode 集合由 `abi::op_supported` 唯一定义，probe 与 issue 共用 |
| `kernel::io_uring::file` | ring fd 的 `DeviceFile`：poll/epoll、mmap 与匿名 inode 标签 |
| `memory::DeviceBacking` | ring 页的物理所有权，以及 `atomic_u32`/`write` 这两个唯一的共享页访问入口 |
| `syscall::io_uring` | `io_uring_setup/enter/register` 的参数解码、user-copy、临时 signal mask 与 errno；opcode 的 prep（`ops`）；`Now`/`Wait` 与显式 task 的执行（`exec`）；interest 登记（`wait`）；reactor 与取消（`worker`）；不解释 ring 内存 |
| `syscall::fs::io` | `perform_read/perform_write`：`read(2)`、`readv(2)` 与 io_uring 共用的读写核心、记账与 inotify 通知；`nowait` 是唯一的 per-IO 阻塞开关 |

规范：Linux `v7.1`，见 [标准基线](../standards-baseline.md) 的 `include/uapi/linux/io_uring.h` 与 `io_uring/` 实现语义。
依赖只经 `fs::chardev::DeviceFile`；`fs` 不得依赖 `io_uring`，取回 ring 使用 `Any` 向下转型。

## Interface

- ring 头部布局：`sq.head@0`、`sq.tail@64`、`cq.head@128`、`cq.tail@192`、各 mask/entries/flags/dropped/overflow 紧随其后，CQE 数组从
  320 起，SQ array 在 CQE 数组之后。偏移经 `io_uring_params` 告知用户态，不得被假定固定。
- 所有 head/tail/flags 访问都经 `AtomicU32`：内核写 CQE 后 release 写 `cq.tail`，读 `sq.tail` 用 acquire，写 `sq.head` 用 release。
- `submit` 把 SQE 整体复制出共享页（`IORING_FEAT_SUBMIT_STABLE`）后才推进 head；init/prep 失败的 SQE 计入已提交并停止本次提交，
  下标越界的 SQE 只推进 head、累加 `sq_dropped` 并停止，二者与 Linux 一致。
- 未实现但已定义的 opcode、`BUFFER_SELECT` 完成为 `-EOPNOTSUPP`，≥ `IORING_OP_LAST` 或未知 flag 位、非零 `personality` 为 `-EINVAL`；probe 对未实现
  opcode 如实标为不支持。
- 链的组装规则（`chain`，对照 Linux `io_submit_sqe`/`io_submit_fail_init`）属于 ring 域，成员的执行属于 `syscall`：prep 失败的链整体不执行，失败成员保留自己的错误、其余 `-ECANCELED`；
  成员的“失败”不等于 `res < 0`（读写短传输失败，`ETIME_SUCCESS` 的超时不失败），失败使 `CQE_SKIP_SUCCESS` 失效。
- DRAIN 由 `Ring::admit`/`may_start` 的序号表达：DRAIN 链要求自己是最早的未完成链，普通链要求之前没有未完成的 DRAIN 链。链的 `Unit` 在链完成或被丢弃时释放，
  新增任何“链结束”的路径都必须经由丢弃 `Unit`，否则其后的 DRAIN 链永远不开始。
- 内核线程可以访问其代表的用户任务的 fd 表：文件表查找使用当前执行体自己预置的 waiter（`TaskControlBlock::file_wait`），用户线程与内核线程各有一份；
  worker 执行链后继时依赖这一点，缺失时 worker 第一次 `fd_get`/`fd_close` 即 panic。
- socket 的 accept/connect 核心（`accept_once`/`connect_begin`）与地址编解码都显式接收 task，不读取 `current_task()`；新增 socket 相关 opcode 不得绕开它们。
- reactor 线程从不睡眠：任何可能睡眠在磁盘上的 opcode 路径必须是 `Wait::FileRead`（不阻塞的 page-cache 填页）或 `Wait::Lane`（阻塞车道）。
  `Interest::Block` 的完成通知（块读、车道作业）必须经 `Waiter::notifier()` 置位 `notified` 并唤醒 reactor；future 在每次 `step` 之前 `clear_notified`，之后才可能等待，
  否则通知要么丢失要么让 reactor 空转。
- 读写核心不得在 `nowait` 为真时睡眠；会睡眠的路径改返回 `EAGAIN`。新增一个会阻塞的 OFD 种类时，必须同时决定它在 `can_block` 里的归类，
  并确认 `add_interest` 能为它登记等待；否则它的请求会被挂起却永不唤醒。
- 挂起请求只持有提交线程的 `Weak<TaskControlBlock>`，强引用会延长其 TCB 寿命；future 只在每次重试的一小段持有强引用、不跨 `.await`，升级失败则以 `-ECANCELED` 结束。
- 挂起请求的 future 不得持有任何锁跨越 `.await`（worker 是单线程 reactor，持锁挂起会让提交者与取消者死锁）；future 一律用 fallible 分配创建，OOM 以 `-ENOMEM` 完成而不是 panic。
- worker 是 reactor 而非 waker 驱动的执行器（等待来源是 task 睡眠/唤醒式的 `WAIT_REGISTRY`，没有回调）：新增会等待的请求种类时，必须新增一个 `Wait` 变体，让它的 future 把等待条件登记为一种 `Interest`，并在 `wait` 里为它构造 `wait_for_poll` key，
  否则 future 被挂起后没有任何东西会重新 poll 它。
- 带计数的 `TIMEOUT` 统计“除 timeout 自身外已发布的 CQE 数”（`Ring::events_posted`）；timeout 的完成必须经 `complete_timeout`，否则两个 timeout 会互相推进对方。
- 凭据：请求使用提交者提交时刻的凭据（`IORING_FEAT_CUR_PERSONALITY`），没有注册 personality。

## Failure and cleanup

- `io_uring_setup` 任一分配失败返回 `ENOMEM` 且不安装 fd；fd 在 params 写回成功后才发布，`EFAULT` 时没有残留。
- ring 页创建时清零，映射给用户态前不得含有旧内核数据。
- CQ 溢出链表有 8 倍 CQ 容量的硬上限；超限的 CQE 被丢弃，计入 `cq_overflow`，并通过下一次等待中的 `EBADR` 通知一次。
- 取消：`DeviceFile::release`（最后一个 OFD 引用释放，含进程退出关闭 fd）置 ring 关闭标志并唤醒内核线程，它丢弃该 ring 的全部挂起请求且不发布 CQE，
  同时释放请求持有的目标 fd；缺失该步骤，关闭 ring 后被挂起的 pipe 读端会一直保持打开，对端永远得不到 `EPIPE`/EOF。
- worker 登记等待失败（分配）时取消所有正在等待的成员（它们以 `-ECANCELED` 完成），而不是留下永不完成的请求；启动链时的分配失败（future、槽、登记表、接纳序号）以 `-ENOMEM` 完成尚未完成的成员。
- 关闭 fd 不撤销已存在的映射：映射持有 `DeviceBacking`，ring 对象释放后页仍可读，但内核不再写入。
- 阻塞等待只被 CQE、`EXT_ARG` 超时（`ETIME`）或可交付 signal（`EINTR`）结束；临时 sigmask 在 `EINTR` 以外的返回路径恢复，
  `EINTR` 时由 signal frame 恢复，与 `epoll_pwait` 相同。

## 新增 global / lock / atomic 的所有权

- `Ring::submit_lock`（`TaskMutex`）：串行化 SQ 消费与 head 提交。缺失时两个线程同时 enter 会重复提交同一个 SQE。
- `Ring::cq`（`spin::Mutex`）：CQ 发布与溢出链表。缺失时并发 post 会覆盖同一个 CQE 槽或打乱完成顺序。
- `Shared::slots`（`spin::Mutex<Vec<Arc<Slot>>>`）与 `SHARED`（`spin::Once`）：所有挂起链的唯一登记表，同时是取消者查找目标的依据；只有 worker 移除条目。boot 时发布一次。
  缺失时挂起的链没有执行者，取消者找不到目标。`Slot::future`（`spin::Mutex<Option<_>>`）只被 worker（以及登记前的提交者）持有来 poll，不被取消者触碰，所以 poll 中可以回头取消别的链。
- `Waiter`（`wait` 模块，`Arc<WaiterInner>`，内含 `spin::Mutex<State>`）：future 登记等待条件、正在运行的成员、取消标记与 `LINK_TIMEOUT` 的 deadline（`link_deadline`/`timed_out`），每条链一个，只在 worker、创建它的提交者与取消者之间短暂持有且不跨 `.await`。
  缺失时 worker 不知道该等什么，取消者无从标记目标。
- `Ring::units`（`spin::Mutex<Units>`）：已接纳未完成的链及其 DRAIN 标志，序号单调。缺失时 DRAIN 没有顺序依据。`Ring::drain_disabled`/`drain_next`（`AtomicBool`）：Linux `IO_RING_F_DRAIN_DISABLED`/`DRAIN_NEXT`，只在 `submit_lock` 下被提交路径修改。
- `lane::Pool`（`spin::Mutex`，`lane::POOL` 经 `spin::Once` 在 boot 发布）：阻塞车道的共享 FIFO 与 worker 登记表（含空闲标志，只在该锁下改变，使“入队”与“worker 置空闲”不会交错成作业在队列里而全部 worker 都在睡）；
  worker 最多 `MAX_WORKERS` 个、按需创建、不退出，缺失上限时大量慢作业各占一个内核栈；`Worker::wake`（每 worker 一对 notification pipe）只由认领它的提交者置位；`Ticket::result`（`spin::Mutex<Option<isize>>`）是作业结果槽，
  提交者与车道线程各持一个引用，车道在引用计数为 1（提交者已丢弃票据）时跳过尚未开始的作业。缺失时可能睡眠的作业只能在 reactor 上执行。
- `WaiterInner::notified`（`AtomicBool`）：见上，块读与车道作业完成时在 deferred 上下文里无锁置位。
- 车道作业以提交线程的 `Weak<TaskControlBlock>` 执行；文件表查找使用执行体自己预置的 waiter（见上），内核线程因此可以访问被代表任务的 fd 表。
- `Ring::closed`（`AtomicBool`）：ring fd 最后一个引用已释放。缺失时关闭 ring 不会取消挂起请求。
- `Ring::generation`（`AtomicU64`）：epoll edge 判定，取 `next_readiness_generation` 的最大值；缺失时 ET epoll 会漏报。
- `DeviceBacking::atomic_u32` 的 unsafe：偏移 4 字节对齐且在同一页内，frame 由 backing 持有，引用生命周期受 `&self` 约束。
