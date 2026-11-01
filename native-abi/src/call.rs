//! native 调用编号与参数。
//!
//! 每个编号的文档按“参数 → 输出 → 错误”描述。所有调用都可能返回 `ERR_BAD_HANDLE`（句柄无效）、
//! `ERR_WRONG_TYPE`（对象类型不符）、`ERR_ACCESS_DENIED`（rights 不足）、`ERR_INVALID_POINTER`
//! （用户指针不可访问）与 `ERR_NO_MEMORY`，下文不再重复。

// ---------------------------------------------------------------------------------------------
// 句柄与对象
// ---------------------------------------------------------------------------------------------

/// 关闭句柄；关闭最后一个引用时销毁对象。参数：`handle`。
pub const HANDLE_CLOSE: usize = 0;

/// 以不大于原句柄的 rights 复制句柄。
///
/// 参数：`handle`、`rights`（`rights::SAME` 表示保持）、`out: *mut Handle`。
pub const HANDLE_DUPLICATE: usize = 1;

/// 以不大于原句柄的 rights 替换句柄：原句柄关闭，返回新句柄。
///
/// 参数：`handle`、`rights`、`out: *mut Handle`。
pub const HANDLE_REPLACE: usize = 2;

/// 等待对象任一 signal 满足或截止时间到达。
///
/// 参数：`handle`、`signals`、`deadline`、`observed: *mut u32`（可为 0）。
/// 错误：`ERR_TIMED_OUT`、`ERR_CANCELED`（句柄被关闭）、`ERR_NOT_SUPPORTED`（对象不可等待）。
pub const OBJECT_WAIT_ONE: usize = 3;

/// 注册一次性异步等待：signal 满足时向 port 投递 `packet::SIGNAL_ONE`。
///
/// 参数：`handle`、`port`、`key`、`signals`。错误：`ERR_NOT_SUPPORTED`。
pub const OBJECT_WAIT_ASYNC: usize = 4;

/// 清除并置位对象的用户 signal（event 另可置 `SIGNALED`）。
///
/// 参数：`handle`、`clear_mask`、`set_mask`。错误：`ERR_INVALID_ARGS`（不可由用户修改的位）。
pub const OBJECT_SIGNAL: usize = 5;

/// 读取对象信息。
///
/// 参数：`handle`、`topic`（[`crate::info`]）、`buffer`、`capacity`、`actual: *mut usize`。
/// 错误：`ERR_BUFFER_TOO_SMALL`（写回所需大小）、`ERR_NOT_SUPPORTED`。
pub const OBJECT_GET_INFO: usize = 6;

/// 设置对象名称（进程、线程、VMO）。参数：`handle`、`name`、`length`（≤ 32）。
pub const OBJECT_SET_NAME: usize = 7;

// ---------------------------------------------------------------------------------------------
// IPC
// ---------------------------------------------------------------------------------------------

/// 创建一对相连的 channel 端点。参数：`out0: *mut Handle`、`out1: *mut Handle`。
pub const CHANNEL_CREATE: usize = 8;

/// 写入一条消息；所传句柄从调用者句柄表移出（失败时同样关闭）。
///
/// 参数：`handle`、`bytes`、`num_bytes`、`handles: *const Handle`、`num_handles`。
/// 错误：`ERR_OUT_OF_RANGE`（超过消息上限）、`ERR_PEER_CLOSED`、`ERR_INVALID_ARGS`（传递自身或重复句柄）、
/// `ERR_NO_RESOURCES`（对端积压过多）。
pub const CHANNEL_WRITE: usize = 9;

/// 读取队首消息。
///
/// 参数：`handle`、`bytes`、`bytes_capacity`、`handles: *mut Handle`、`handles_capacity`、
/// `actual: *mut ChannelActual`。
/// 错误：`ERR_SHOULD_WAIT`、`ERR_PEER_CLOSED`、`ERR_BUFFER_TOO_SMALL`（写回所需大小，消息保留）。
pub const CHANNEL_READ: usize = 10;

/// 写请求并同步等待带相同事务号的回复。
///
/// 参数：`handle`、`args: *const ChannelCallArgs`、`deadline`、`actual: *mut ChannelActual`。
/// 错误：`CHANNEL_WRITE`/`CHANNEL_READ` 的错误，以及 `ERR_TIMED_OUT`、`ERR_CANCELED`。
pub const CHANNEL_CALL: usize = 11;

/// 创建 event。参数：`out: *mut Handle`。
pub const EVENT_CREATE: usize = 12;

/// 创建 port。参数：`out: *mut Handle`。
pub const PORT_CREATE: usize = 13;

/// 取出一个 packet，必要时等待。参数：`port`、`deadline`、`packet: *mut PortPacket`。错误：`ERR_TIMED_OUT`。
pub const PORT_WAIT: usize = 14;

/// 投递一个 `packet::USER`。参数：`port`、`key`、`payload`。
pub const PORT_QUEUE: usize = 15;

// ---------------------------------------------------------------------------------------------
// 内存
// ---------------------------------------------------------------------------------------------

/// 创建零内容 VMO。参数：`size`、`options`（[`crate::vmo`]）、`out: *mut Handle`。
pub const VMO_CREATE: usize = 16;

/// 从 VMO 读取。参数：`vmo`、`buffer`、`offset`、`length`。错误：`ERR_OUT_OF_RANGE`、`ERR_IO`（pager 失败）。
pub const VMO_READ: usize = 17;

/// 写入 VMO。参数同 `VMO_READ`。
pub const VMO_WRITE: usize = 18;

/// 读取 VMO 字节大小。参数：`vmo`、`out: *mut u64`。
pub const VMO_GET_SIZE: usize = 19;

/// 改变可调整大小 VMO 的大小。参数：`vmo`、`size`。错误：`ERR_NOT_SUPPORTED`、`ERR_BAD_STATE`（越界页被固定）。
pub const VMO_SET_SIZE: usize = 20;

/// 对页区间执行操作。参数：`vmo`、`op`（[`crate::vmo`] `OP_*`）、`offset`、`length`。
pub const VMO_OP_RANGE: usize = 21;

/// 创建子 VMO。参数：`vmo`、`options`（`CHILD_*`）、`offset`、`size`、`out: *mut Handle`。
/// 错误：`ERR_NOT_SUPPORTED`（连续/物理 VMO 不支持 COW 子对象）。
pub const VMO_CREATE_CHILD: usize = 22;

/// 创建物理连续、常驻的 VMO（DMA）。参数：`bti`、`size`、`out: *mut Handle`。
pub const VMO_CREATE_CONTIGUOUS: usize = 23;

/// 创建引用 MMIO 区间的 VMO。参数：`resource`（覆盖该区间）、`physical`、`size`、`out: *mut Handle`。
pub const VMO_CREATE_PHYSICAL: usize = 24;

/// 在 VMAR 内映射 VMO。
///
/// 参数：`vmar`、`options`（[`crate::vm`]）、`vmar_offset`（`SPECIFIC` 时有效）、`vmo`、`vmo_offset`、
/// `length`、`address: *mut u64`。
/// 错误：`ERR_INVALID_ARGS`（未对齐、地址冲突）、`ERR_OUT_OF_RANGE`、`ERR_NO_RESOURCES`（VMAR 无空隙）。
pub const VMAR_MAP: usize = 25;

/// 解除映射。参数：`vmar`、`address`、`length`。错误：`ERR_BAD_STATE`（区间与子 VMAR 部分重叠）。
pub const VMAR_UNMAP: usize = 26;

/// 改变映射权限。参数：`vmar`、`options`（权限位）、`address`、`length`。
pub const VMAR_PROTECT: usize = 27;

/// 分配子 VMAR。参数：`parent`、`options`、`offset`、`size`、`out: *mut Handle`、`address: *mut u64`。
pub const VMAR_ALLOCATE: usize = 28;

/// 销毁 VMAR 及其全部映射。参数：`vmar`。
pub const VMAR_DESTROY: usize = 29;

/// 读取另一进程的内存。参数：`process`、`address`、`buffer`、`length`、`actual: *mut usize`。
pub const PROCESS_READ_MEMORY: usize = 30;

/// 写入另一进程的内存。参数同 `PROCESS_READ_MEMORY`。
pub const PROCESS_WRITE_MEMORY: usize = 31;

/// 创建 pager。参数：`out: *mut Handle`。
pub const PAGER_CREATE: usize = 32;

/// 创建由 pager 支撑的 VMO；缺页请求以 `packet::PAGE_REQUEST` 投递到 `port`。
///
/// 参数：`pager`、`options`、`port`、`key`、`size`、`out: *mut Handle`。
pub const PAGER_CREATE_VMO: usize = 33;

/// 把 `aux` 中的页移入 pager VMO。参数：`pager`、`vmo`、`offset`、`length`、`aux`、`aux_offset`。
pub const PAGER_SUPPLY_PAGES: usize = 34;

/// pager 操作（失败、回写开始、查询脏区间）。参数：`pager`、`op`（[`crate::pager`]）、`vmo`、`offset`、
/// `length`、`out: *mut [u64; 2]`（`OP_QUERY_DIRTY` 写回第一段脏区间）。
pub const PAGER_OP: usize = 35;

// ---------------------------------------------------------------------------------------------
// 任务
// ---------------------------------------------------------------------------------------------

/// 创建子 Job。参数：`parent`、`out: *mut Handle`。
pub const JOB_CREATE: usize = 36;

/// 创建进程及其根 VMAR。
///
/// 参数：`job`、`name`、`name_length`、`options`（[`crate::process`]）、`process: *mut Handle`、
/// `vmar: *mut Handle`。`SHARED` 选项创建只管理共享区的共享进程。
pub const PROCESS_CREATE: usize = 37;

/// 创建与 `shared` 共享高半区的进程；根 VMAR 是其私有低半区。
///
/// 参数：`shared`、`name`、`name_length`、`process: *mut Handle`、`vmar: *mut Handle`。
pub const PROCESS_CREATE_SHARED: usize = 38;

/// 启动进程的首个线程，并把 `handle_arg` 转移进目标进程作为第一个参数。
///
/// 参数：`process`、`thread`、`entry`、`stack`、`handle_arg`（可为 `HANDLE_INVALID`）、`arg1`。
/// 错误：`ERR_BAD_STATE`（进程已启动或已终止）。
pub const PROCESS_START: usize = 39;

/// 终止调用者所在进程。参数：`code`。不返回。
pub const PROCESS_EXIT: usize = 40;

/// 在进程中创建未启动的线程。参数：`process`、`name`、`name_length`、`out: *mut Handle`。
pub const THREAD_CREATE: usize = 41;

/// 启动线程。参数：`thread`、`entry`、`stack`、`arg0`、`arg1`。错误：`ERR_BAD_STATE`。
pub const THREAD_START: usize = 42;

/// 终止调用线程；进程最后一个线程退出即终止进程。不返回。
pub const THREAD_EXIT: usize = 43;

/// 终止进程、线程或 Job（含其子树）。参数：`handle`。
pub const TASK_KILL: usize = 44;

/// 设置线程权重等级。参数：`thread`、`priority`（`0..40`，小者权重大）。
pub const THREAD_SET_PRIORITY: usize = 45;

/// 为进程或 Job 创建异常 channel；未处理异常以 [`crate::ExceptionReport`] 消息投递。
///
/// 参数：`task`、`out: *mut Handle`。错误：`ERR_ALREADY_BOUND`（已有异常 channel）。
pub const TASK_CREATE_EXCEPTION_CHANNEL: usize = 46;

/// 处理完异常：`resume` 为 1 时线程从（可能被修改的）寄存器继续，否则交给上一级处理者。
///
/// 参数：`exception`（随消息传来的句柄）、`resume`、`state: *const RegisterState`（可为 0）。
pub const EXCEPTION_RESOLVE: usize = 47;

// ---------------------------------------------------------------------------------------------
// 同步与 restricted mode
// ---------------------------------------------------------------------------------------------

/// 若 `*address == expected` 则阻塞直到被唤醒或截止时间到达。
///
/// 参数：`address`、`expected`、`deadline`、`options`（[`crate::futex`]）。
/// 错误：`ERR_BAD_STATE`（值不等）、`ERR_TIMED_OUT`。
pub const FUTEX_WAIT: usize = 48;

/// 唤醒至多 `count` 个等待者。参数：`address`、`count`、`options`、`woken: *mut u32`（可为 0）。
pub const FUTEX_WAKE: usize = 49;

/// 唤醒 `wake_count` 个，并把至多 `requeue_count` 个等待者移到 `target`。
///
/// 参数：`address`、`wake_count`、`expected`、`target`、`requeue_count`、`options`。
/// 错误：`ERR_BAD_STATE`（值不等）。
pub const FUTEX_REQUEUE: usize = 50;

/// 绑定调用线程的 restricted 状态 VMO（布局为 [`crate::RestrictedState`]）。参数：`out: *mut Handle`。
pub const RESTRICTED_BIND_STATE: usize = 51;

/// 解除调用线程的 restricted 状态绑定。无参数。
pub const RESTRICTED_UNBIND_STATE: usize = 52;

/// 以绑定状态中的寄存器进入 restricted mode。
///
/// 参数：`vector`（退出时 normal mode 的入口）、`context`（退出时第一个参数）。成功时不返回到调用点：
/// restricted 代码执行 syscall 指令、发生不能由地址空间解决的异常或被 kick 时，normal mode 寄存器恢复为
/// 进入时的值，PC 置为 `vector`，第一个参数为 `context`，第二个参数为 [`crate::restricted`] 原因码。
/// 错误：`ERR_BAD_STATE`（未绑定状态）、`ERR_INVALID_ARGS`（状态中的寄存器非法）。
pub const RESTRICTED_ENTER: usize = 53;

/// 让线程尽快退出 restricted mode；线程在 normal mode 时，下一次 `RESTRICTED_ENTER` 立即以 KICK 返回。
/// 参数：`thread`。
pub const RESTRICTED_KICK: usize = 54;

// ---------------------------------------------------------------------------------------------
// 驱动原语
// ---------------------------------------------------------------------------------------------

/// 从父 resource 切出子 resource。参数：`parent`、`kind`（[`crate::resource`]）、`base`、`size`、`out: *mut Handle`。
pub const RESOURCE_CREATE: usize = 55;

/// 创建中断对象。参数：`resource`（覆盖该向量）、`vector`、`options`（[`crate::interrupt`]）、`out: *mut Handle`。
pub const INTERRUPT_CREATE: usize = 56;

/// 等待中断触发。参数：`interrupt`、`deadline`、`timestamp: *mut i64`（可为 0）。
/// 错误：`ERR_TIMED_OUT`、`ERR_CANCELED`（中断被销毁）。
pub const INTERRUPT_WAIT: usize = 57;

/// 确认中断并重新允许触发（电平触发中断在确认前保持屏蔽）。参数：`interrupt`。
pub const INTERRUPT_ACK: usize = 58;

/// 把中断绑定到 port：每次触发投递 `packet::INTERRUPT`。参数：`interrupt`、`port`、`key`。
pub const INTERRUPT_BIND: usize = 59;

/// 创建 IOMMU 对象。参数：`resource`（root）、`kind`（[`crate::iommu`]）、`descriptor`、`length`、`out: *mut Handle`。
pub const IOMMU_CREATE: usize = 60;

/// 为一个 DMA 发起者创建 BTI。参数：`iommu`、`stream_id`、`out: *mut Handle`。
pub const BTI_CREATE: usize = 61;

/// 固定 VMO 区间并取得设备地址。
///
/// 参数：`bti`、`options`（`bti::*`）、`vmo`、`offset`、`length`、`addresses: *mut u64`
/// （容量为 `length / PAGE_SIZE`）、`pmt: *mut Handle`。
pub const BTI_PIN: usize = 62;

/// 解除固定并撤销设备地址。参数：`pmt`。
pub const PMT_UNPIN: usize = 63;

// ---------------------------------------------------------------------------------------------
// 系统
// ---------------------------------------------------------------------------------------------

/// 向内核日志写入一条记录。参数：`severity`（0 debug..3 error）、`bytes`、`length`（≤ 1024，超出截断）。
pub const DEBUGLOG_WRITE: usize = 64;

/// 按序号读取内核日志。
///
/// 参数：`resource`（root）、`sequence`、`buffer: *mut DebuglogRecord`、`deadline`（`DEADLINE_INFINITE`
/// 阻塞，0 不等待）。错误：`ERR_SHOULD_WAIT`（无新记录）、`ERR_OUT_OF_RANGE`（已被覆盖，
/// 记录头的 `sequence` 写回最老可读序号）。
pub const DEBUGLOG_READ: usize = 65;

/// 读取调试串口输入。参数：`resource`（root）、`buffer`、`capacity`、`deadline`、`actual: *mut usize`。
pub const SERIAL_READ: usize = 66;

/// 单调时钟纳秒。参数：`out: *mut i64`。
pub const CLOCK_GET_MONOTONIC: usize = 67;

/// UTC 纳秒。参数：`out: *mut i64`。
pub const CLOCK_GET_UTC: usize = 68;

/// 设置 UTC。参数：`resource`（root）、`utc_ns`。
pub const CLOCK_SET_UTC: usize = 69;

/// 睡眠到截止时间。参数：`deadline`。错误：`ERR_CANCELED`（线程被终止）。
pub const NANOSLEEP: usize = 70;

/// 读取密码学安全随机字节。参数：`buffer`、`length`。
pub const CPRNG_DRAW: usize = 71;

/// 向 CPRNG 混入熵。参数：`buffer`、`length`（≤ 256）。
pub const CPRNG_ADD_ENTROPY: usize = 72;

/// 读取系统信息。参数：`topic`（[`crate::system`]）、`buffer`、`capacity`、`actual: *mut usize`。
pub const SYSTEM_GET_INFO: usize = 73;

/// 关机或重启。参数：`resource`（root）、`action`（[`crate::system`] `POWER_*`）。
pub const SYSTEM_POWER: usize = 74;

/// 编号上界（不含）。
pub const COUNT: usize = 75;
